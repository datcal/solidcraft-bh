//! bh: process plug-ins (datcal/solidcraft-bh, see bh/BH.md and bh/plugins/PROTOCOL.md).
//!
//! A plug-in folder with a `bh.json` next to its `plugin.json` names a program in that folder.
//! SolidCraft starts it and the two talk in JSON lines over the program's stdin and stdout:
//! the program puts a button on the application bar (`button`), describes a panel that
//! SolidCraft draws (`panel`), runs commands through the queue the control channel uses
//! (`request` → `response`, docs/control-protocol.md) and hears the panel's clicks and edits
//! (`event`). The program does everything else itself (network, files), so this file is the only
//! place SolidCraft changes for it, besides three calls: [`pump`] in `SolveApp::logic`, [`show`] at
//! the end of `SolveApp::ui` and [`app_bar`] in `toolbar::app_bar`.
//!
//! Plug-ins come from the user's plug-ins folder ([`crate::plugins::plugins_dir`]), then from the
//! ones packaged with the app (`<exe>/../share/solidcraft/plugins`), then from
//! `SOLIDCRAFT_BH_PLUGINS` (development). The first folder with an id wins, and an id turned off
//! in Preferences › Plugins is not started.

#[cfg(target_arch = "wasm32")]
use egui::Rect;

#[cfg(target_arch = "wasm32")]
use crate::SolveApp;

#[cfg(target_arch = "wasm32")]
pub fn pump(_app: &mut SolveApp, _ctx: &egui::Context) {}

#[cfg(target_arch = "wasm32")]
pub fn show(_app: &mut SolveApp, _ctx: &egui::Context) {}

/// The browser build has no plug-ins: the whole span stays for the document tabs.
#[cfg(target_arch = "wasm32")]
pub fn app_bar(_app: &mut SolveApp, _ui: &mut egui::Ui, _span: Rect) -> Option<f32> {
    None
}

#[cfg(not(target_arch = "wasm32"))]
pub use desktop::{app_bar, pump, show};

#[cfg(not(target_arch = "wasm32"))]
mod desktop {
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::io::{BufRead, BufReader, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio};
    use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};

    use egui::text::{LayoutJob, TextFormat, TextWrapping};
    use egui::{Align2, Color32, FontId, Rect, RichText, Sense, Stroke, pos2, vec2};
    use serde::Deserialize;
    use serde_json::{Value, json};

    use crate::SolveApp;
    use crate::theme::Tokens;

    /// A line from a plug-in longer than this is dropped.
    const MAX_LINE: usize = 4 * 1024 * 1024;
    /// Requests a plug-in may have waiting at once.
    const MAX_PENDING: usize = 64;
    /// Panel elements drawn at most, and how deep rows and columns nest.
    const MAX_ELEMENTS: usize = 2000;
    const MAX_DEPTH: usize = 8;

    /// `bh.json`: the program and its application-bar button.
    #[derive(Clone, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Spec {
        /// The program, relative to the plug-in folder (`.exe` is added on Windows).
        run: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        button: Button,
    }

    #[derive(Clone, Default, Deserialize)]
    struct Button {
        #[serde(default)]
        label: String,
        /// An icon of the app's own set (`folder`, `open`, …); empty for none.
        #[serde(default)]
        icon: String,
        #[serde(default)]
        tooltip: String,
    }

    enum Msg {
        Line(Value),
        Bad(String),
        Closed,
    }

    struct Found {
        id: String,
        name: String,
        dir: PathBuf,
        spec: Spec,
    }

    struct Running {
        found: Found,
        child: Option<Child>,
        to: Sender<String>,
        from: Receiver<Msg>,
        button: Button,
        panel: Value,
        open: bool,
        /// Text fields being edited, by id, with the `reset` number the plug-in last gave them.
        fields: HashMap<String, (String, i64)>,
        /// Requests waiting for the app: the plug-in's id, the method, the reply.
        pending: Vec<(Value, String, Receiver<Value>)>,
        exited: Option<String>,
        textures: HashMap<String, Option<egui::TextureHandle>>,
    }

    impl Drop for Running {
        fn drop(&mut self) {
            if let Some(mut child) = self.child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    #[derive(Default)]
    struct Host {
        started: bool,
        plugins: Vec<Running>,
    }

    thread_local! {
        static HOST: RefCell<Host> = RefCell::new(Host::default());
    }

    fn roots() -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Some(dir) = crate::plugins::plugins_dir() {
            roots.push(dir);
        }
        if let Some(bin) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf)) {
            roots.push(bin.join("../share/solidcraft/plugins"));
        }
        if let Some(extra) = std::env::var_os("SOLIDCRAFT_BH_PLUGINS") {
            roots.push(PathBuf::from(extra));
        }
        roots
    }

    /// The process plug-ins in `roots`, first id wins.
    fn discover(roots: &[PathBuf]) -> Vec<Found> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for root in roots {
            let (plugins, _) = crate::plugins::scan(root);
            for plugin in plugins {
                let dir = root.join(&plugin.id);
                let Ok(text) = std::fs::read_to_string(dir.join("bh.json")) else { continue };
                if !seen.insert(plugin.id.clone()) {
                    continue;
                }
                match serde_json::from_str::<Spec>(&text) {
                    Ok(spec) => out.push(Found { id: plugin.id, name: plugin.name, dir, spec }),
                    Err(e) => log::warn!("plug-in {}: bh.json: {e}", plugin.id),
                }
            }
        }
        out
    }

    /// Where a plug-in keeps its own files: writable even when the plug-in ships inside the app.
    fn data_dir(id: &str) -> Option<PathBuf> {
        solidcraft_engine::recovery::data_dir().map(|d| d.join("plugin-data").join(id))
    }

    fn spawn(found: Found, ctx: &egui::Context) -> Result<Running, (Found, String)> {
        let mut program = found.dir.join(&found.spec.run);
        if cfg!(windows) && program.extension().is_none() {
            program.set_extension("exe");
        }
        // The program must be inside its plug-in folder.
        let inside = match (found.dir.canonicalize(), program.canonicalize()) {
            (Ok(dir), Ok(prog)) => prog.starts_with(&dir).then_some(prog),
            _ => None,
        };
        let Some(program) = inside else {
            let e = format!("{} is not a program in the plug-in folder", found.spec.run);
            return Err((found, e));
        };
        let data = data_dir(&found.id);
        if let Some(data) = &data {
            let _ = std::fs::create_dir_all(data);
        }
        let mut command = Command::new(&program);
        command
            .args(&found.spec.args)
            .current_dir(&found.dir)
            .env("SOLIDCRAFT_PLUGIN_ID", &found.id)
            .env("SOLIDCRAFT_PLUGIN_DIR", &found.dir)
            .env("SOLIDCRAFT_VERSION", crate::update::current_version())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        if let Some(data) = &data {
            command.env("SOLIDCRAFT_PLUGIN_DATA", data);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                let e = format!("could not start {}: {e}", program.display());
                return Err((found, e));
            }
        };
        let (Some(stdout), Some(mut stdin)) = (child.stdout.take(), child.stdin.take()) else {
            let _ = child.kill();
            return Err((found, "no pipes to the program".into()));
        };
        let (tx, from) = channel();
        let wake = ctx.clone();
        let id = found.id.clone();
        let _ = std::thread::Builder::new().name(format!("plugin-{id}-out")).spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line.len() > MAX_LINE => {
                        let _ = tx.send(Msg::Bad("a line longer than 4 MiB".into()));
                    }
                    Ok(_) if line.trim().is_empty() => continue,
                    Ok(_) => {
                        let msg = serde_json::from_str::<Value>(line.trim()).map_or_else(|e| Msg::Bad(e.to_string()), Msg::Line);
                        if tx.send(msg).is_err() {
                            return;
                        }
                    }
                }
                wake.request_repaint();
            }
            let _ = tx.send(Msg::Closed);
            wake.request_repaint();
        });
        let (to, lines) = channel::<String>();
        let _ = std::thread::Builder::new().name(format!("plugin-{}-in", found.id)).spawn(move || {
            for line in lines {
                if writeln!(stdin, "{line}").and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        let start = json!({"event": {"type": "start", "version": crate::update::current_version(), "language": crate::i18n::language()}});
        let _ = to.send(start.to_string());
        let button = Button {
            label: if found.spec.button.label.is_empty() { found.name.clone() } else { found.spec.button.label.clone() },
            ..found.spec.button.clone()
        };
        Ok(Running {
            found,
            child: Some(child),
            to,
            from,
            button,
            panel: Value::Null,
            open: false,
            fields: HashMap::new(),
            pending: Vec::new(),
            exited: None,
            textures: HashMap::new(),
        })
    }

    impl Host {
        fn start(&mut self, app: &mut SolveApp, ctx: &egui::Context) {
            for found in discover(&roots()) {
                if !crate::plugins::enabled(app, &found.id) {
                    continue;
                }
                match spawn(found, ctx) {
                    Ok(running) => self.plugins.push(running),
                    Err((found, e)) => app.set_status(format!("{}: {e}", found.name), true),
                }
            }
        }
    }

    impl Running {
        fn send(&self, v: Value) {
            let _ = self.to.send(v.to_string());
        }

        fn event(&self, e: Value) {
            self.send(json!({ "event": e }));
        }

        fn set_open(&mut self, open: bool) {
            if self.open != open {
                self.open = open;
                self.event(json!({"type": if open { "open" } else { "close" }}));
            }
        }

        fn pump(&mut self, app: &mut SolveApp) {
            let msgs: Vec<Msg> = self.from.try_iter().take(256).collect();
            for msg in msgs {
                match msg {
                    Msg::Line(v) => self.handle(app, v),
                    Msg::Bad(e) => log::warn!("plug-in {}: {e}", self.found.id),
                    Msg::Closed => {
                        let code = self.child.as_mut().and_then(|c| c.wait().ok()).map_or("closed".to_string(), |s| s.to_string());
                        self.child = None;
                        self.exited = Some(code);
                    }
                }
            }
            let mut answered = Vec::new();
            self.pending.retain(|(id, method, rx)| match rx.try_recv() {
                Ok(reply) => {
                    answered.push((id.clone(), method.clone(), reply));
                    false
                }
                Err(TryRecvError::Empty) => true,
                Err(TryRecvError::Disconnected) => {
                    answered.push((id.clone(), method.clone(), json!({"ok": false, "error": "the request was dropped"})));
                    false
                }
            });
            for (id, method, mut reply) in answered {
                // The desktop app keeps the open design's file in the session, which
                // `document.inspect` does not report: add it as `file`.
                if method == "document.inspect"
                    && let Some(result) = reply.get_mut("result").and_then(Value::as_object_mut)
                {
                    result.insert("file".into(), json!(app.session.path));
                }
                if let Some(o) = reply.as_object_mut() {
                    o.insert("id".into(), id);
                }
                self.send(json!({ "response": reply }));
            }
        }

        fn handle(&mut self, app: &mut SolveApp, v: Value) {
            if let Some(b) = v.get("button") {
                let s = |k: &str| b.get(k).and_then(Value::as_str).map(str::to_string);
                if let Some(label) = s("label") {
                    self.button.label = label.chars().take(48).collect();
                }
                if let Some(icon) = s("icon") {
                    self.button.icon = icon;
                }
                if let Some(tooltip) = s("tooltip") {
                    self.button.tooltip = tooltip;
                }
            }
            if let Some(panel) = v.get("panel") {
                self.panel = panel.clone();
            }
            if let Some(open) = v.get("open").and_then(Value::as_bool) {
                self.open = open;
            }
            if let Some(status) = v.get("status") {
                let text = status.get("text").and_then(Value::as_str).unwrap_or_default();
                if !text.is_empty() {
                    app.set_status(format!("{}: {text}", self.found.name), status.get("error").and_then(Value::as_bool).unwrap_or(false));
                }
            }
            if let Some(r) = v.get("request") {
                let id = r.get("id").cloned().unwrap_or(Value::Null);
                let Some(method) = r.get("method").and_then(Value::as_str) else {
                    self.send(json!({"response": {"id": id, "ok": false, "error": "missing `method`"}}));
                    return;
                };
                if self.pending.len() >= MAX_PENDING {
                    self.send(json!({"response": {"id": id, "ok": false, "error": "too many requests waiting"}}));
                    return;
                }
                let params = r.get("params").cloned().unwrap_or_else(|| json!({}));
                let (req, rx) = crate::control::ControlRequest::new(method, params);
                app.control_held.push_back(req);
                self.pending.push((id, method.to_string(), rx));
            }
        }

        fn window(&mut self, ctx: &egui::Context) {
            let t = Tokens::get();
            let title = self.panel.get("title").and_then(Value::as_str).unwrap_or(&self.found.name).to_string();
            let width = self.panel.get("width").and_then(Value::as_f64).unwrap_or(720.0) as f32;
            let body = self.panel.get("body").cloned().unwrap_or(Value::Null);
            let mut events = Vec::new();
            let mut used = HashSet::new();
            let at = pos2(ctx.content_rect().right() - 12.0, crate::toolbar::APP_BAR_HEIGHT + 8.0);
            let closed = crate::ui_kit::Window::new(egui::Id::new(("bh_plugin_panel", &self.found.id)), &self.button.icon, &title)
                .cards((width / t.form_card_width.max(1.0)).clamp(1.0, 16.0))
                .page_rows(32.0)
                .placed(Align2::RIGHT_TOP, at)
                .show_body(ctx, |ui| {
                    if let Some(code) = &self.exited {
                        ui.label(RichText::new(format!("The plug-in stopped ({code}).")).color(t.error));
                    }
                    let mut draw = Draw { fields: &mut self.fields, textures: &mut self.textures, used: &mut used, events: &mut events, count: 0 };
                    draw.list(ui, &body, 0);
                });
            self.textures.retain(|path, _| used.contains(path));
            for e in events {
                self.event(e);
            }
            if closed {
                self.set_open(false);
            }
        }
    }

    struct Draw<'a> {
        fields: &'a mut HashMap<String, (String, i64)>,
        textures: &'a mut HashMap<String, Option<egui::TextureHandle>>,
        used: &'a mut HashSet<String>,
        events: &'a mut Vec<Value>,
        count: usize,
    }

    fn text_of<'v>(v: &'v Value, key: &str) -> &'v str {
        v.get(key).and_then(Value::as_str).unwrap_or_default()
    }

    fn colour(name: &str) -> Color32 {
        let t = Tokens::get();
        match name {
            "dim" => t.text_dim,
            "error" => t.error,
            "warning" => t.warning,
            "accent" => t.accent,
            _ => t.text,
        }
    }

    /// One line of `text`, cut with an ellipsis at `width`.
    fn one_line(text: &str, size: f32, color: Color32, width: f32, strong: bool) -> LayoutJob {
        let font = if strong { crate::fonts::semibold(size) } else { FontId::proportional(size) };
        let mut job = LayoutJob::single_section(text.to_string(), TextFormat { font_id: font, color, ..Default::default() });
        job.wrap = TextWrapping::truncate_at_width(width.max(1.0));
        job
    }

    impl Draw<'_> {
        fn list(&mut self, ui: &mut egui::Ui, list: &Value, depth: usize) {
            for el in list.as_array().into_iter().flatten() {
                self.element(ui, el, depth);
            }
        }

        fn element(&mut self, ui: &mut egui::Ui, el: &Value, depth: usize) {
            self.count += 1;
            if self.count > MAX_ELEMENTS || depth > MAX_DEPTH {
                return;
            }
            let t = Tokens::get();
            let id = text_of(el, "id").to_string();
            let flag = |k: &str| el.get(k).and_then(Value::as_bool).unwrap_or(false);
            let number = |k: &str| el.get(k).and_then(Value::as_f64).filter(|x| x.is_finite());
            match text_of(el, "type") {
                "row" => {
                    ui.horizontal_wrapped(|ui| self.list(ui, &el["items"], depth + 1));
                }
                "column" => {
                    ui.vertical(|ui| self.list(ui, &el["items"], depth + 1));
                }
                "heading" => {
                    ui.label(RichText::new(text_of(el, "text")).strong().size(t.font_heading));
                }
                "text" => {
                    let mut text = RichText::new(text_of(el, "text")).color(colour(text_of(el, "color")));
                    if flag("small") {
                        text = text.size(t.font_small);
                    }
                    if flag("strong") {
                        text = text.strong();
                    }
                    ui.add(egui::Label::new(text).wrap());
                }
                "button" => {
                    let kind = if flag("primary") {
                        crate::ui_kit::Button::Primary
                    } else if flag("small") {
                        crate::ui_kit::Button::Small
                    } else {
                        crate::ui_kit::Button::Normal
                    };
                    let enabled = el.get("enabled").and_then(Value::as_bool).unwrap_or(true);
                    let response = ui.add_enabled_ui(enabled, |ui| crate::ui_kit::button(ui, text_of(el, "label"), kind)).inner;
                    let response = match text_of(el, "tooltip") {
                        "" => response,
                        tip => response.on_hover_text(tip),
                    };
                    if response.clicked() {
                        self.events.push(json!({"type": "click", "id": id}));
                    }
                }
                "input" => {
                    // The plug-in sets the text by sending a new `reset` number with a `value`.
                    let reset = el.get("reset").and_then(Value::as_i64).unwrap_or(0);
                    let entry = self.fields.entry(id.clone()).or_insert_with(|| (text_of(el, "value").to_string(), reset));
                    if entry.1 != reset {
                        *entry = (text_of(el, "value").to_string(), reset);
                    }
                    let edit = egui::TextEdit::singleline(&mut entry.0).hint_text(text_of(el, "hint")).desired_width(number("width").unwrap_or(220.0) as f32);
                    let response = ui.add(edit);
                    if response.changed() {
                        self.events.push(json!({"type": "input", "id": id, "value": entry.0}));
                    }
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.events.push(json!({"type": "submit", "id": id, "value": entry.0}));
                    }
                }
                "separator" => {
                    ui.separator();
                }
                "space" => {
                    ui.add_space(number("size").unwrap_or(8.0).clamp(0.0, 200.0) as f32);
                }
                "progress" => {
                    let bar = egui::ProgressBar::new(number("value").unwrap_or(0.0).clamp(0.0, 1.0) as f32).desired_width(number("width").unwrap_or(240.0) as f32);
                    ui.add(if text_of(el, "text").is_empty() { bar } else { bar.text(text_of(el, "text")) });
                }
                "cards" => self.cards(ui, el, &id),
                _ => {}
            }
        }

        /// A picture from a file, decoded once per path (a plug-in writes a new name for a new picture).
        fn texture(&mut self, ctx: &egui::Context, path: &str) -> Option<egui::TextureHandle> {
            if path.is_empty() {
                return None;
            }
            self.used.insert(path.to_string());
            self.textures
                .entry(path.to_string())
                .or_insert_with(|| {
                    let image = image::open(path).ok()?.to_rgba8();
                    let size = [image.width() as usize, image.height() as usize];
                    let color = egui::ColorImage::from_rgba_unmultiplied(size, image.as_raw());
                    Some(ctx.load_texture(format!("bh-plugin:{path}"), color, egui::TextureOptions::LINEAR))
                })
                .clone()
        }

        /// A grid of cards: a picture, a title, a smaller line, an optional badge and small action
        /// buttons. A click on a card is `{"type":"card","action":"open"}`, on an action its id.
        fn cards(&mut self, ui: &mut egui::Ui, el: &Value, list: &str) {
            let t = Tokens::get();
            let items: Vec<Value> = el.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            if items.is_empty() {
                return;
            }
            let gap = 10.0;
            let min_width = el.get("min_width").and_then(Value::as_f64).unwrap_or(200.0).clamp(80.0, 800.0) as f32;
            let room = ui.available_width().max(min_width);
            let columns = (((room + gap) / (min_width + gap)).floor() as usize).max(1);
            let width = (room - gap * (columns as f32 - 1.0)) / columns as f32;
            let picture = (width * 10.0 / 16.0).round();
            let height = picture + 50.0;
            for row in items.chunks(columns) {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for item in row {
                        self.count += 1;
                        if self.count > MAX_ELEMENTS {
                            return;
                        }
                        let item_id = text_of(item, "id").to_string();
                        let (rect, response) = ui.allocate_exact_size(vec2(width, height), Sense::click());
                        let selected = item.get("selected").and_then(Value::as_bool).unwrap_or(false);
                        let painter = ui.painter_at(rect);
                        let radius = f32::from(t.radius_sm) + 2.0;
                        painter.rect_filled(rect, radius, t.panel_header);
                        let frame = Rect::from_min_size(rect.min, vec2(width, picture));
                        painter.rect_filled(frame, egui::CornerRadius { nw: radius as u8, ne: radius as u8, sw: 0, se: 0 }, Color32::from_rgb(0xdd, 0xe1, 0xe7));
                        if let Some(texture) = self.texture(ui.ctx(), text_of(item, "image")) {
                            let size = texture.size_vec2();
                            let scale = (frame.width() / size.x).min(frame.height() / size.y);
                            let shown = Rect::from_center_size(frame.center(), size * scale);
                            painter.image(texture.id(), shown, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                        } else {
                            crate::icons::paint_on(&painter, Rect::from_center_size(frame.center(), vec2(40.0, 40.0)), "folder", Some(false), 0.35);
                        }
                        let text_width = width - 20.0;
                        let title = one_line(text_of(item, "title"), t.font_body, t.text, text_width, true);
                        painter.galley(pos2(rect.left() + 10.0, frame.bottom() + 7.0), ui.fonts_mut(|f| f.layout_job(title)), t.text);
                        let line = one_line(text_of(item, "subtitle"), t.font_small, t.text_dim, text_width, false);
                        painter.galley(pos2(rect.left() + 10.0, frame.bottom() + 28.0), ui.fonts_mut(|f| f.layout_job(line)), t.text_dim);
                        let badge = text_of(item, "badge");
                        if !badge.is_empty() {
                            let galley = ui.fonts_mut(|f| f.layout_no_wrap(badge.to_string(), FontId::proportional(t.font_small), t.on_accent));
                            let pill = Rect::from_min_size(rect.min + vec2(8.0, 8.0), galley.size() + vec2(14.0, 4.0));
                            painter.rect_filled(pill, pill.height() / 2.0, t.accent);
                            painter.galley(pill.min + vec2(7.0, 2.0), galley, t.on_accent);
                        }
                        // Actions sit at the top right of the picture, shown on hover (and always
                        // when one asks to be, e.g. a confirmation).
                        let mut right = frame.right() - 6.0;
                        let mut on_action = false;
                        for action in item.get("actions").and_then(Value::as_array).into_iter().flatten().take(4) {
                            let always = action.get("always").and_then(Value::as_bool).unwrap_or(false);
                            if !(always || response.hovered() || ui.rect_contains_pointer(rect)) {
                                continue;
                            }
                            let label = text_of(action, "label");
                            let danger = action.get("danger").and_then(Value::as_bool).unwrap_or(false);
                            let galley = ui.fonts_mut(|f| f.layout_no_wrap(label.to_string(), FontId::proportional(t.font_small), Color32::WHITE));
                            let button = Rect::from_min_size(pos2(right - galley.size().x - 14.0, frame.top() + 6.0), galley.size() + vec2(14.0, 8.0));
                            right = button.left() - 6.0;
                            let hit = ui.interact(button, ui.id().with(("bh_card_action", list, &item_id, text_of(action, "id"))), Sense::click());
                            let fill = match (danger, hit.hovered()) {
                                (true, true) => t.error,
                                (true, false) => t.error.gamma_multiply(0.85),
                                (false, true) => Color32::from_black_alpha(220),
                                (false, false) => Color32::from_black_alpha(170),
                            };
                            painter.rect_filled(button, f32::from(t.radius_sm), fill);
                            painter.galley(button.min + vec2(7.0, 4.0), galley, Color32::WHITE);
                            on_action |= hit.hovered();
                            if hit.clicked() {
                                self.events.push(json!({"type": "card", "list": list, "id": item_id, "action": text_of(action, "id")}));
                            }
                        }
                        let border = if selected {
                            Stroke::new(2.0, t.accent)
                        } else if response.hovered() {
                            Stroke::new(1.0, t.accent)
                        } else {
                            Stroke::new(1.0, t.border)
                        };
                        painter.rect_stroke(rect, radius, border, egui::StrokeKind::Inside);
                        let tip = text_of(item, "tooltip");
                        let response = if tip.is_empty() { response } else { response.on_hover_text(tip) };
                        if response.clicked() && !on_action {
                            self.events.push(json!({"type": "card", "list": list, "id": item_id, "action": "open"}));
                        }
                    }
                });
                ui.add_space(gap);
            }
        }
    }

    /// Start the plug-ins (first frame), read what they sent, queue their requests and answer the
    /// ones the app has run. Called from `SolveApp::logic` before the control queue is drained.
    pub fn pump(app: &mut SolveApp, ctx: &egui::Context) {
        HOST.with_borrow_mut(|host| {
            if !host.started {
                host.started = true;
                host.start(app, ctx);
            }
            for plugin in &mut host.plugins {
                plugin.pump(app);
            }
            if host.plugins.iter().any(|p| !p.pending.is_empty()) {
                ctx.request_repaint_after(std::time::Duration::from_millis(30));
            }
        });
    }

    /// The open plug-in panels. Called at the end of `SolveApp::ui`.
    pub fn show(_app: &mut SolveApp, ctx: &egui::Context) {
        HOST.with_borrow_mut(|host| {
            for plugin in host.plugins.iter_mut().filter(|p| p.open) {
                plugin.window(ctx);
            }
        });
    }

    /// The plug-ins' buttons, right-aligned in `span` of the application bar (left of the Update
    /// pill). Returns where they start, for the document tabs to stop before (None: no buttons).
    pub fn app_bar(_app: &mut SolveApp, ui: &mut egui::Ui, span: Rect) -> Option<f32> {
        HOST.with_borrow_mut(|host| {
            let t = Tokens::get();
            let mut right = span.right();
            let mut restart = None;
            for (i, plugin) in host.plugins.iter_mut().enumerate().rev() {
                let label = if plugin.exited.is_some() { format!("{} ⚠", plugin.button.label) } else { plugin.button.label.clone() };
                let icon = !plugin.button.icon.is_empty();
                let text_x = if icon { 29.0 } else { 10.0 };
                let galley = ui.fonts_mut(|f| f.layout_job(one_line(&label, t.font_button, t.app_bar_text, 200.0, false)));
                let width = text_x + galley.size().x + 11.0;
                if right - width < span.left() {
                    break;
                }
                let rect = Rect::from_min_size(pos2(right - width, span.center().y - 13.0), vec2(width, 26.0));
                right = rect.left() - 6.0;
                let response = ui.interact(rect, ui.id().with(("bh_plugin_button", &plugin.found.id)), Sense::click());
                response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &label));
                crate::scenario::publish_target(&format!("titlebar:plugin:{}", plugin.found.id), rect);
                crate::scenario::publish_handle(&format!("titlebar:plugin:{}", plugin.found.id), rect.center());
                let hot = response.hovered() || response.has_focus() || plugin.open;
                ui.painter().rect_filled(rect, t.radius_sm, if hot { t.app_bar_hover_button } else { t.app_bar });
                ui.painter().rect_stroke(rect, t.radius_sm, Stroke::new(1.0, t.app_bar_separator), egui::StrokeKind::Inside);
                if icon {
                    let glyph = Rect::from_center_size(pos2(rect.left() + 14.0, rect.center().y), vec2(16.0, 16.0));
                    crate::icons::paint_app_bar(ui.painter(), glyph, &plugin.button.icon, 1.0);
                }
                ui.painter().galley(pos2(rect.left() + text_x, rect.center().y - galley.size().y / 2.0), galley, t.app_bar_text);
                let tip = if let Some(code) = &plugin.exited {
                    format!("{} stopped ({code}); click to start it again", plugin.found.name)
                } else if plugin.button.tooltip.is_empty() {
                    plugin.found.name.clone()
                } else {
                    plugin.button.tooltip.clone()
                };
                if response.on_hover_text(tip).clicked() {
                    if plugin.exited.is_some() {
                        restart = Some(i);
                    } else {
                        let open = !plugin.open;
                        plugin.set_open(open);
                    }
                }
            }
            if let Some(i) = restart {
                let old = host.plugins.remove(i);
                let found = Found { id: old.found.id.clone(), name: old.found.name.clone(), dir: old.found.dir.clone(), spec: old.found.spec.clone() };
                drop(old);
                match spawn(found, ui.ctx()) {
                    Ok(mut running) => {
                        running.set_open(true);
                        host.plugins.insert(i, running);
                    }
                    Err((found, e)) => log::warn!("plug-in {}: {e}", found.id),
                }
            }
            (right < span.right()).then_some(right + 6.0)
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn plugin_dir(root: &Path, id: &str, bh: &str) -> PathBuf {
            let dir = root.join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("plugin.json"), format!(r#"{{"id":"{id}","name":"{id} name","version":"1.0.0"}}"#)).unwrap();
            std::fs::write(dir.join("bh.json"), bh).unwrap();
            dir
        }

        #[test]
        fn a_folder_with_bh_json_is_a_process_plugin_and_the_first_root_wins() {
            let base = std::env::temp_dir().join(format!("bh-plugins-discover-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let (user, bundled) = (base.join("user"), base.join("bundled"));
            plugin_dir(&user, "designs", r#"{"run":"designs","button":{"label":"Designs","icon":"folder"}}"#);
            plugin_dir(&bundled, "designs", r#"{"run":"other"}"#);
            plugin_dir(&bundled, "broken", r#"{"run":"x","unknown":1}"#);
            std::fs::create_dir_all(bundled.join("plain")).unwrap();
            std::fs::write(bundled.join("plain/plugin.json"), r#"{"id":"plain","name":"Plain","version":"1"}"#).unwrap();
            let found = discover(&[user.clone(), bundled]);
            assert_eq!(found.len(), 1, "a broken bh.json and a plugin.json without one are not process plug-ins");
            assert_eq!(found[0].id, "designs");
            assert_eq!(found[0].spec.run, "designs");
            assert_eq!(found[0].dir, user.join("designs"));
            assert_eq!(found[0].spec.button.icon, "folder");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_program_outside_its_folder_is_refused() {
            let base = std::env::temp_dir().join(format!("bh-plugins-outside-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let dir = plugin_dir(&base, "escape", r#"{"run":"../../../bin/sh"}"#);
            let found = Found { id: "escape".into(), name: "Escape".into(), dir, spec: Spec { run: "../../../bin/sh".into(), args: vec![], button: Button::default() } };
            let Err((_, e)) = spawn(found, &egui::Context::default()) else { panic!("a program outside the folder must not start") };
            assert!(e.contains("not a program in the plug-in folder"), "{e}");
            let _ = std::fs::remove_dir_all(&base);
        }

        /// A shell script plug-in: it asks for a command and echoes the response as its button label.
        #[cfg(unix)]
        #[test]
        fn a_plugin_runs_a_command_and_hears_the_response() {
            use std::os::unix::fs::PermissionsExt;
            let base = std::env::temp_dir().join(format!("bh-plugins-run-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let dir = plugin_dir(&base, "probe", r#"{"run":"probe.sh"}"#);
            let script = dir.join("probe.sh");
            std::fs::write(
                &script,
                "#!/bin/sh\nread start\necho '{\"panel\":{\"title\":\"Probe\",\"body\":[]},\"request\":{\"id\":1,\"method\":\"engine.execute\",\"params\":{\"command\":\"solid.box\",\"params\":{\"length\":10,\"width\":10,\"height\":10}}}}'\nread response\ncase \"$response\" in *'\"id\":1'*'\"ok\":true'*) echo '{\"button\":{\"label\":\"ok\"}}' ;; *) echo '{\"button\":{\"label\":\"failed\"}}' ;; esac\nread rest\n",
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut app = SolveApp::new(solidcraft_engine::Session::default(), crate::Services::default());
            let ctx = egui::Context::default();
            let found = discover(std::slice::from_ref(&base)).pop().unwrap();
            let mut running = spawn(found, &ctx).unwrap_or_else(|(_, e)| panic!("{e}"));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while running.button.label != "ok" && std::time::Instant::now() < deadline {
                running.pump(&mut app);
                // What `SolveApp::drain_control` does with the held queue, minus the waiting rules.
                while let Some(req) = app.control_held.pop_front() {
                    app.run_control(&ctx, req);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert_eq!(running.panel["title"], "Probe");
            assert_eq!(running.button.label, "ok", "the plug-in got an ok response to its request id 1");
            let bodies = app.session.execute("document.inspect", &json!({})).unwrap()["bodies"].as_array().map_or(0, Vec::len);
            assert_eq!(bodies, 1, "the box was made");
            drop(running);
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}
