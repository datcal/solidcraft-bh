//! designs: a SolidCraft process plug-in (bh/plugins/PROTOCOL.md) for a design store: an HTTPS
//! server behind Cloudflare Access (its API: bh/plugins/designs/README.md).
//!
//! The panel lists the store's designs as cards with their pictures. Opening one downloads it to
//! `<data>/open/<name>.sc` and opens it with `doc.open`; from then on every save of that file (the
//! panel's Save, the app's own Save, Ctrl+S) is uploaded to the store, with a copy in
//! `.history/<name>/<time>.sc`. A design that is not from the store goes there under a name, saved
//! with `file.save_as` into the same folder. The store's address and the Cloudflare Access service
//! token are only in `<data>/settings.json` on the user's computer (the panel's Settings edits it).

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// How often the open design is checked (its name, unsaved changes, a save to upload).
const INSPECT_EVERY: Duration = Duration::from_secs(2);
/// How often the list is fetched again while the panel is open.
const LIST_EVERY: Duration = Duration::from_secs(30);
const MAX_DESIGN: u64 = 200 * 1024 * 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
struct Settings {
    url: String,
    client_id: String,
    client_secret: String,
}

struct Design {
    name: String,
    modified: Option<chrono::DateTime<chrono::Utc>>,
    size: u64,
    picture: Option<PathBuf>,
}

/// What a request's response is for.
enum Waiting {
    Inspect,
    Open(String),
    Save(String),
}

#[derive(Default)]
struct Plugin {
    data: PathBuf,
    settings: Option<Settings>,
    designs: Vec<Design>,
    query: String,
    /// The store design the active document is: its file is `<data>/open/<name>.sc`.
    open_name: Option<String>,
    dirty: bool,
    status: String,
    error: bool,
    naming: bool,
    name_value: String,
    /// The name a second Save overwrites, after the first one said it exists.
    overwrite: Option<String>,
    reset: i64,
    confirm_delete: Option<String>,
    editing_settings: bool,
    form: Settings,
    /// Each opened file's modification time when it was last downloaded or uploaded.
    synced: HashMap<String, SystemTime>,
    next_id: u64,
    waiting: HashMap<u64, Waiting>,
    panel_open: bool,
    listed_at: Option<Instant>,
    list_again_at: Option<Instant>,
    inspected_at: Option<Instant>,
    button: String,
}

fn send(v: Value) {
    println!("{v}");
}

/// A name the store and every file system accept: accented letters spelled in ASCII, anything
/// else but letters, digits, `-` and `_` turned into `-`.
fn clean(name: &str) -> String {
    let name = name.trim();
    let name = name.strip_suffix(".sc").or_else(|| name.strip_suffix(".solidcraft")).unwrap_or(name);
    let mut out = String::new();
    for c in name.chars() {
        let c = match c {
            'ç' => 'c',
            'ğ' => 'g',
            'ı' => 'i',
            'ö' => 'o',
            'ş' => 's',
            'ü' => 'u',
            'Ç' => 'C',
            'Ğ' => 'G',
            'İ' => 'I',
            'Ö' => 'O',
            'Ş' => 'S',
            'Ü' => 'U',
            c if c.is_ascii_alphanumeric() || c == '_' || c == '-' => c,
            _ => '-',
        };
        if !(c == '-' && out.ends_with('-')) {
            out.push(c);
        }
    }
    out.trim_matches('-').chars().take(80).collect()
}

fn ago(t: chrono::DateTime<chrono::Utc>) -> String {
    let s = (chrono::Utc::now() - t).num_seconds();
    match s {
        ..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        86400..172800 => "yesterday".into(),
        172800..604800 => format!("{} days ago", s / 86400),
        _ => t.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string(),
    }
}

fn size(n: u64) -> String {
    if n < 1024 * 1024 { format!("{} KB", (n / 1024).max(1)) } else { format!("{:.1} MB", n as f64 / 1048576.0) }
}

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

impl Plugin {
    fn new() -> Self {
        let data = std::env::var_os("SOLIDCRAFT_PLUGIN_DATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
        let _ = std::fs::create_dir_all(data.join("open"));
        let _ = std::fs::create_dir_all(data.join("previews"));
        let settings = std::fs::read_to_string(data.join("settings.json")).ok().and_then(|t| serde_json::from_str::<Settings>(&t).ok());
        let mut plugin = Plugin { data, editing_settings: settings.is_none(), settings, ..Default::default() };
        // Files already here are taken as in step with the store; a later save of one is uploaded.
        if let Ok(entries) = std::fs::read_dir(plugin.data.join("open")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if let (Some(name), Some(time)) = (plugin.name_of(&path), modified(&path)) {
                    plugin.synced.insert(name, time);
                }
            }
        }
        plugin.form = plugin.settings.clone().unwrap_or_default();
        plugin
    }

    fn file(&self, name: &str) -> PathBuf {
        self.data.join("open").join(format!("{name}.sc"))
    }

    /// The store name of a file in `<data>/open`.
    fn name_of(&self, path: &Path) -> Option<String> {
        let dir = self.data.join("open");
        (path.parent()? == dir && path.extension()? == "sc").then(|| path.file_stem()?.to_str().map(str::to_string)).flatten()
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.status = text.into();
        self.error = error;
    }

    // ---- the store ------------------------------------------------------------------------------

    fn http(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<Vec<u8>, String> {
        let Some(s) = &self.settings else { return Err("Enter the store's address and access token under Settings first.".into()) };
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(120)))
            .build()
            .into();
        let url = format!("{}{path}", s.url.trim_end_matches('/'));
        let (id, secret) = (s.client_id.as_str(), s.client_secret.as_str());
        let result = match (method, body) {
            ("PUT", Some(body)) => agent.put(&url).header("CF-Access-Client-Id", id).header("CF-Access-Client-Secret", secret).send(body),
            ("DELETE", _) => agent.delete(&url).header("CF-Access-Client-Id", id).header("CF-Access-Client-Secret", secret).call(),
            _ => agent.get(&url).header("CF-Access-Client-Id", id).header("CF-Access-Client-Secret", secret).call(),
        };
        let mut response = result.map_err(|e| format!("The store can't be reached: {e}"))?;
        let status = response.status().as_u16();
        // Cloudflare Access answers a wrong or expired token with a redirect to its sign-in.
        if (300..400).contains(&status) || status == 401 || status == 403 {
            return Err("The store refused the access token (see Settings).".into());
        }
        if !(200..300).contains(&status) {
            return Err(format!("The store answered {status} ({method} {path})."));
        }
        response.body_mut().with_config().limit(MAX_DESIGN).read_to_vec().map_err(|e| format!("Reading from the store failed: {e}"))
    }

    fn list(&mut self) {
        self.listed_at = Some(Instant::now());
        let listing = match self.http("GET", "/designs/", None).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string())) {
            Ok(v) => v,
            Err(e) => return self.say(e, true),
        };
        let mut designs = Vec::new();
        for f in listing.as_array().into_iter().flatten() {
            let file = f.get("name").and_then(Value::as_str).unwrap_or_default();
            let Some(name) = file.strip_suffix(".sc").filter(|_| f.get("type").and_then(Value::as_str) == Some("file")) else { continue };
            let modified = f.get("mtime").and_then(Value::as_str).and_then(|t| chrono::DateTime::parse_from_rfc2822(t).ok()).map(|t| t.to_utc());
            let size = f.get("size").and_then(Value::as_u64).unwrap_or(0);
            designs.push(Design { name: name.to_string(), modified, size, picture: None });
        }
        designs.sort_by(|a, b| b.modified.cmp(&a.modified));
        for design in &mut designs {
            design.picture = self.picture(&design.name, design.modified);
        }
        self.designs = designs;
    }

    /// The design's picture (drawn by the store a few seconds after a save), kept as
    /// `<data>/previews/<name>--<time>.png` so a new picture gets a new name.
    fn picture(&self, name: &str, modified: Option<chrono::DateTime<chrono::Utc>>) -> Option<PathBuf> {
        let stamp = modified.map_or(0, |t| t.timestamp());
        let dir = self.data.join("previews");
        let path = dir.join(format!("{name}--{stamp}.png"));
        if path.exists() {
            return Some(path);
        }
        let bytes = self.http("GET", &format!("/designs/.previews/{name}.png"), None).ok()?;
        // Older pictures of this design are dropped.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(&format!("{name}--")) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        std::fs::write(&path, bytes).ok()?;
        Some(path)
    }

    fn upload(&mut self, name: &str) -> Result<(), String> {
        let path = self.file(name);
        let time = modified(&path);
        let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        self.http("PUT", &format!("/designs/{name}.sc"), Some(&bytes))?;
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        self.http("PUT", &format!("/designs/.history/{name}/{stamp}.sc"), Some(&bytes))?;
        if let Some(time) = time {
            self.synced.insert(name.to_string(), time);
        }
        // The store draws the new picture a few seconds later.
        self.list_again_at = Some(Instant::now() + Duration::from_secs(8));
        self.say(format!("Saved: {name} ({})", chrono::Local::now().format("%H:%M")), false);
        Ok(())
    }

    // ---- the app --------------------------------------------------------------------------------

    fn request(&mut self, method: &str, params: Value, waiting: Waiting) {
        self.next_id += 1;
        self.waiting.insert(self.next_id, waiting);
        send(json!({"request": {"id": self.next_id, "method": method, "params": params}}));
    }

    fn command(&mut self, command: &str, params: Value, waiting: Waiting) {
        self.request("engine.execute", json!({"command": command, "params": params}), waiting);
    }

    fn open(&mut self, name: &str) {
        if self.open_name.as_deref() != Some(name) {
            let path = self.file(name);
            let bytes = match self.http("GET", &format!("/designs/{name}.sc"), None) {
                Ok(b) => b,
                Err(e) => return self.say(e, true),
            };
            let partial = path.with_extension("sc.part");
            if let Err(e) = std::fs::write(&partial, bytes).and_then(|_| std::fs::rename(&partial, &path)) {
                return self.say(format!("{}: {e}", path.display()), true);
            }
            if let Some(time) = modified(&path) {
                self.synced.insert(name.to_string(), time);
            }
        }
        self.say(format!("Opening {name}…"), false);
        let path = self.file(name).display().to_string();
        self.command("doc.open", json!({"path": path}), Waiting::Open(name.to_string()));
    }

    /// Save: the open store design is saved in place (and uploaded once written); any other
    /// design asks for a name first.
    fn save(&mut self) {
        match self.open_name.clone() {
            Some(name) => {
                self.say(format!("Saving {name}…"), false);
                self.command("file.save", json!({"overwrite": true}), Waiting::Save(name));
            }
            None => self.ask_name(),
        }
    }

    fn ask_name(&mut self) {
        self.naming = true;
        self.overwrite = None;
        self.name_value = self.open_name.clone().unwrap_or_default();
        self.reset += 1;
    }

    fn save_as(&mut self) {
        let name = clean(&self.name_value);
        if name.is_empty() {
            return self.say("Type a name.", true);
        }
        let exists = self.designs.iter().any(|d| d.name == name);
        if exists && self.open_name.as_deref() != Some(&name) && self.overwrite.as_deref() != Some(&name) {
            self.overwrite = Some(name.clone());
            return self.say(format!("\"{name}\" is already in the store. Press Save again to replace it."), true);
        }
        self.naming = false;
        self.overwrite = None;
        self.say(format!("Saving {name}…"), false);
        let path = self.file(&name).display().to_string();
        self.command("file.save_as", json!({"path": path, "overwrite": true}), Waiting::Save(name));
    }

    fn delete(&mut self, name: &str) {
        self.confirm_delete = None;
        match self.http("DELETE", &format!("/designs/{name}.sc"), None) {
            Ok(_) => self.say(format!("Deleted {name} (its history stays in the store)"), false),
            Err(e) => self.say(e, true),
        }
        self.list();
    }

    fn save_settings(&mut self) {
        let mut s = self.form.clone();
        s.url = s.url.trim().trim_end_matches('/').to_string();
        s.client_id = s.client_id.trim().to_string();
        s.client_secret = s.client_secret.trim().to_string();
        if !s.url.starts_with("https://") || s.client_id.is_empty() || s.client_secret.is_empty() {
            return self.say("The address must start with https://, and the client ID and secret can't be empty.", true);
        }
        let path = self.data.join("settings.json");
        let text = serde_json::to_string_pretty(&s).unwrap_or_default();
        if let Err(e) = std::fs::write(&path, text) {
            return self.say(format!("{}: {e}", path.display()), true);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        self.settings = Some(s);
        self.editing_settings = false;
        self.say("Settings saved.", false);
        self.list();
    }

    // ---- messages -------------------------------------------------------------------------------

    fn message(&mut self, v: Value) {
        if let Some(e) = v.get("event") {
            self.event(e);
        }
        if let Some(r) = v.get("response") {
            self.response(r);
        }
    }

    fn event(&mut self, e: &Value) {
        let s = |k: &str| e.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let (id, value) = (s("id"), s("value"));
        match s("type").as_str() {
            "open" => {
                self.panel_open = true;
                self.status.clear();
                self.list();
            }
            "close" => {
                self.panel_open = false;
                self.naming = false;
                self.confirm_delete = None;
            }
            "click" => match id.as_str() {
                "save" => self.save(),
                "save-as" => self.ask_name(),
                "name-save" => self.save_as(),
                "name-cancel" => self.naming = false,
                "refresh" => self.list(),
                "settings" => {
                    self.editing_settings = !self.editing_settings;
                    self.form = self.settings.clone().unwrap_or_default();
                    self.reset += 1;
                }
                "settings-save" => self.save_settings(),
                "settings-cancel" => self.editing_settings = false,
                _ => {}
            },
            "input" => match id.as_str() {
                "search" => self.query = value,
                "name" => self.name_value = value,
                "settings-url" => self.form.url = value,
                "settings-id" => self.form.client_id = value,
                "settings-secret" => self.form.client_secret = value,
                _ => {}
            },
            "submit" => match id.as_str() {
                "name" => {
                    self.name_value = value;
                    self.save_as();
                }
                "settings-secret" => self.save_settings(),
                _ => {}
            },
            "card" => match s("action").as_str() {
                "open" => self.open(&id),
                "delete" => self.confirm_delete = Some(id),
                "delete-confirm" => self.delete(&id),
                "cancel" => self.confirm_delete = None,
                _ => {}
            },
            _ => return,
        }
        self.render();
    }

    fn response(&mut self, r: &Value) {
        let Some(waiting) = r.get("id").and_then(Value::as_u64).and_then(|id| self.waiting.remove(&id)) else { return };
        let ok = r.get("ok").and_then(Value::as_bool).unwrap_or(false);
        let error = r.get("error").and_then(Value::as_str).unwrap_or("unknown error").to_string();
        match waiting {
            Waiting::Inspect => {
                if ok {
                    self.inspected(&r["result"]);
                }
                return;
            }
            Waiting::Open(name) if ok => {
                self.open_name = Some(name.clone());
                self.dirty = false;
                self.say(format!("Opened {name}"), false);
            }
            Waiting::Save(name) if ok => {
                self.open_name = Some(name.clone());
                if let Err(e) = self.upload(&name) {
                    self.say(e, true);
                }
                self.list();
            }
            Waiting::Open(name) => self.say(format!("{name} could not be opened: {error}"), true),
            Waiting::Save(name) => self.say(format!("{name} could not be saved: {error}"), true),
        }
        self.render();
    }

    /// The active document (`document.inspect`): which store design it is, whether it has unsaved
    /// changes, and whether one of the opened files was saved since it was last uploaded.
    fn inspected(&mut self, doc: &Value) {
        // `file` is added by the plug-in host (the desktop app's `path` stays empty).
        let path = doc.get("file").or_else(|| doc.get("path")).and_then(Value::as_str).map(PathBuf::from);
        let open_name = path.as_deref().and_then(|p| self.name_of(p));
        let dirty = doc.get("dirty").and_then(Value::as_bool).unwrap_or(false);
        let mut changed = (open_name.clone(), dirty) != (self.open_name.clone(), self.dirty);
        self.open_name = open_name;
        self.dirty = dirty;
        let saved: Vec<String> = self
            .synced
            .iter()
            .filter(|(name, time)| modified(&self.file(name)).is_some_and(|now| now != **time))
            .map(|(name, _)| name.clone())
            .collect();
        for name in saved {
            if let Err(e) = self.upload(&name) {
                self.say(e, true);
            }
            changed = true;
        }
        if changed {
            self.render();
        }
    }

    fn tick(&mut self) {
        let now = Instant::now();
        let waiting_inspect = self.waiting.values().any(|w| matches!(w, Waiting::Inspect));
        if !waiting_inspect && self.inspected_at.is_none_or(|t| now - t >= INSPECT_EVERY) {
            self.inspected_at = Some(now);
            self.request("document.inspect", json!({}), Waiting::Inspect);
        }
        let due = self.list_again_at.is_some_and(|t| now >= t);
        if self.panel_open && self.settings.is_some() && (due || self.listed_at.is_none_or(|t| now - t >= LIST_EVERY)) {
            self.list_again_at = None;
            self.list();
            self.render();
        }
    }

    // ---- what SolidCraft draws ------------------------------------------------------------------

    fn render(&mut self) {
        let label = match &self.open_name {
            Some(name) => format!("Designs: {name}{}", if self.dirty { " •" } else { "" }),
            None => "Designs".to_string(),
        };
        if label != self.button {
            send(json!({"button": {"label": label}}));
            self.button = label;
        }
        let mut body = Vec::new();
        let open = match &self.open_name {
            Some(name) if self.dirty => format!("Open: {name} · unsaved changes (Save or Ctrl+S uploads them)"),
            Some(name) => format!("Open: {name} · same as the store"),
            None => "The open design is not in the store: Save adds it".to_string(),
        };
        body.push(json!({"type": "text", "text": open, "color": "dim", "small": true}));
        body.push(json!({"type": "row", "items": [
            {"type": "input", "id": "search", "hint": "Search…", "width": 200, "value": self.query},
            {"type": "button", "id": "save", "label": "Save", "primary": true, "tooltip": "Ctrl+S saves to the store too"},
            {"type": "button", "id": "save-as", "label": "Save as…"},
            {"type": "button", "id": "refresh", "label": "Refresh"},
            {"type": "button", "id": "settings", "label": "Settings"},
        ]}));
        if self.naming {
            body.push(json!({"type": "row", "items": [
                {"type": "text", "text": "Name in the store:"},
                {"type": "input", "id": "name", "value": self.name_value, "reset": self.reset, "width": 240, "hint": "my-part"},
                {"type": "button", "id": "name-save", "label": "Save", "primary": true},
                {"type": "button", "id": "name-cancel", "label": "Cancel"},
            ]}));
        }
        if self.editing_settings {
            body.push(json!({"type": "separator"}));
            body.push(json!({"type": "text", "text": "Store access: its address and a Cloudflare Access service token. They stay on this computer.", "small": true, "color": "dim"}));
            body.push(json!({"type": "row", "items": [{"type": "text", "text": "Address"}, {"type": "input", "id": "settings-url", "value": self.form.url, "reset": self.reset, "width": 320, "hint": "https://…"}]}));
            body.push(json!({"type": "row", "items": [{"type": "text", "text": "Client ID"}, {"type": "input", "id": "settings-id", "value": self.form.client_id, "reset": self.reset, "width": 320}]}));
            body.push(json!({"type": "row", "items": [{"type": "text", "text": "Client secret"}, {"type": "input", "id": "settings-secret", "value": self.form.client_secret, "reset": self.reset, "width": 320}]}));
            body.push(json!({"type": "row", "items": [
                {"type": "button", "id": "settings-save", "label": "Save settings", "primary": true},
                {"type": "button", "id": "settings-cancel", "label": "Cancel", "enabled": self.settings.is_some()},
            ]}));
            body.push(json!({"type": "separator"}));
        }
        if !self.status.is_empty() {
            body.push(json!({"type": "text", "text": self.status, "small": true, "color": if self.error { "error" } else { "dim" }}));
        }
        body.push(json!({"type": "space", "size": 4}));
        let query = clean(&self.query).to_lowercase();
        let items: Vec<Value> = self
            .designs
            .iter()
            .filter(|d| query.is_empty() || d.name.to_lowercase().contains(&query))
            .map(|d| {
                let open = self.open_name.as_deref() == Some(d.name.as_str());
                let actions = if self.confirm_delete.as_deref() == Some(d.name.as_str()) {
                    json!([{"id": "delete-confirm", "label": "Delete", "danger": true, "always": true}, {"id": "cancel", "label": "Cancel", "always": true}])
                } else {
                    json!([{"id": "delete", "label": "Delete"}])
                };
                json!({
                    "id": d.name,
                    "title": d.name,
                    "subtitle": format!("{} · {}", d.modified.map_or_else(|| "?".to_string(), ago), size(d.size)),
                    "image": d.picture.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
                    "badge": if open { "Open" } else { "" },
                    "selected": open,
                    "tooltip": format!("{} – click to open", d.name),
                    "actions": actions,
                })
            })
            .collect();
        if items.is_empty() {
            let empty = if self.designs.is_empty() { "No designs in the store yet. Save adds the open design." } else { "No design matches the search." };
            body.push(json!({"type": "text", "text": empty, "color": "dim"}));
        } else {
            body.push(json!({"type": "cards", "id": "designs", "min_width": 210, "items": items}));
        }
        send(json!({"panel": {"title": "My designs", "width": 780, "body": body}}));
    }
}

fn main() {
    let (tx, rx) = mpsc::channel::<Option<Value>>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if let Ok(v) = serde_json::from_str::<Value>(&line)
                && tx.send(Some(v)).is_err()
            {
                return;
            }
        }
        let _ = tx.send(None);
    });
    let mut plugin = Plugin::new();
    plugin.render();
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(message)) => plugin.message(message),
            // SolidCraft closed our stdin: it quit, so do we.
            Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        plugin.tick();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_spelled_for_the_store() {
        assert_eq!(clean("Box Test ş"), "Box-Test-s");
        assert_eq!(clean("model-d-template.sc"), "model-d-template");
        assert_eq!(clean(" ÇĞİÖŞÜ / çğıöşü "), "CGIOSU-cgiosu");
        assert_eq!(clean("..//"), "");
    }

    #[test]
    fn only_files_in_the_open_folder_have_a_store_name() {
        let plugin = Plugin { data: PathBuf::from("/data"), ..Default::default() };
        assert_eq!(plugin.name_of(Path::new("/data/open/part.sc")).as_deref(), Some("part"));
        assert_eq!(plugin.name_of(Path::new("/data/open/part.step")), None);
        assert_eq!(plugin.name_of(Path::new("/elsewhere/part.sc")), None);
    }
}
