//! Planned Render interactions, with explicit non-mutating refusals.
use super::CommandSpec;
use crate::{Result, Session, params::bad};
use serde::Deserialize;
use serde_json::{Value, json};

macro_rules! waiting {
    ($id:literal, $label:literal, $tab:literal, $panel:literal, $icon:literal, $reason:literal) => {
        CommandSpec::new($id, $label, |_, _| Err(crate::EngineError::Other(concat!("not supported yet: ", $reason).into())))
            .enabled(|_| Err(concat!("not supported yet: ", $reason).into()))
            .planned("5493")
            .noundo()
            .doc_only()
            .at($tab, $panel)
            .icon($icon)
            .params("No supported parameters. Always refuses without changing the design or history.")
    };
}

pub static COMMANDS: &[CommandSpec] = &[
    CommandSpec::new("render.output.size", "Set Render Output Size", output_size)
        .icon("settings")
        .noundo()
        .doc_only()
        .params("width, height: 1..16384; custom?: bool. Validated Render output draft request; headless echo only, no design edits or job."),
    CommandSpec::new("render.result.view", "View Render Image", result_view)
        .icon("render_image")
        .noundo()
        .doc_only()
        .params("view: model|image|fit|actual. Validated result canvas request; headless echo only, never starts a job."),
    CommandSpec::new("render.result.close", "Close Render Image", close_result)
        .icon("close")
        .key("Escape")
        .noundo()
        .doc_only()
        .params("No parameters. Return to Model Preview; headless echo only, never cancels a job."),
    waiting!("render.lights.add", "Add Light", "RENDER SCENE", "LIGHTS", "render_scene", "named light creation"),
    waiting!("appearance.paint", "Paint Appearance", "RENDER APPEARANCE", "APPEARANCE", "appearance", "product picking and appearance painting"),
    waiting!("appearance.pick_up", "Pick Up Appearance", "RENDER APPEARANCE", "APPEARANCE", "select", "product picking and appearance sampling"),
    waiting!("render.lights.solo", "Solo Light", "RENDER SCENE", "LIGHTS", "render_scene", "light visibility and solo controls"),
    waiting!("render.compare", "Compare Images", "RENDER", "RESULT", "compare", "retained image comparison"),
    waiting!("render.batch", "Render All Shots", "RENDER", "BATCH", "render_image", "the saved-shot render queue"),
];

#[cfg(test)]
#[test]
fn render_workspace_planned_commands_refuse_without_mutation() {
    use serde_json::{Value, json};
    assert!(super::find_command("appearance.replace").is_some_and(|command| command.planned.is_none()));
    let mut session = crate::Session::default();
    session.execute("render.settings.set", &json!({"name":"Studio","set":{"width":800}})).unwrap();
    session.execute("render.settings.set", &json!({"name":"Studio","set":{"width":900}})).unwrap();
    session.execute("edit.undo", &json!({})).unwrap();
    let document = session.doc.clone();
    let before = (session.undo.len(), session.redo.len(), session.revision, session.evaluations, session.is_dirty());
    for command in COMMANDS {
        if command.planned.is_none() {
            continue;
        }
        assert!(!command.info(&session).enabled);
        for params in [json!({}), Value::Null, json!([]), json!({"target":1e300,"name":"hostile"})] {
            let error = session.execute(command.id, &params).unwrap_err().to_string();
            assert!(error.contains("not supported yet"), "{error}");
            assert!(std::sync::Arc::ptr_eq(&document, &session.doc));
            assert_eq!(before, (session.undo.len(), session.redo.len(), session.revision, session.evaluations, session.is_dirty()));
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputSize {
    width: u32,
    height: u32,
    #[serde(default)]
    custom: bool,
}
fn output_size(_: &mut Session, params: &Value) -> Result<Value> {
    let input: OutputSize = serde_json::from_value(params.clone()).map_err(|e| bad("render.output.size", e.to_string()))?;
    if !(1..=16384).contains(&input.width) || !(1..=16384).contains(&input.height) {
        return Err(bad("render.output.size", "dimensions must be in 1..16384"));
    }
    let mut reply = json!({"width":input.width,"height":input.height});
    if input.custom {
        reply["custom"] = json!(true);
    }
    Ok(reply)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultView {
    view: String,
}
fn result_view(_: &mut Session, params: &Value) -> Result<Value> {
    let input: ResultView = serde_json::from_value(params.clone()).map_err(|e| bad("render.result.view", e.to_string()))?;
    if !matches!(input.view.as_str(), "model" | "image" | "fit" | "actual") {
        return Err(bad("render.result.view", "view must be model, image, fit or actual"));
    }
    Ok(json!({"view":input.view}))
}
fn close_result(_: &mut Session, params: &Value) -> Result<Value> {
    if !params.as_object().is_some_and(|p| p.is_empty()) {
        return Err(bad("render.result.close", "expected no parameters"));
    }
    Ok(json!({"view":"model"}))
}

#[cfg(test)]
#[test]
fn output_viewer_requests_are_validated_headless_echoes() {
    let mut s = Session::default();
    let document = s.doc.clone();
    let before = (s.revision, s.undo.len(), s.evaluations);
    for size in [json!({"width":1280,"height":720}), json!({"width":1920,"height":1080})] {
        assert_eq!(s.execute("render.output.size", &size).unwrap(), size);
    }
    let custom = json!({"width":1920,"height":1080,"custom":true});
    assert_eq!(s.execute("render.output.size", &custom).unwrap(), custom);
    assert!(s.execute("render.output.size", &json!({"width":1920,"height":1080,"custom":"yes"})).is_err());
    for view in ["model", "image", "fit", "actual"] {
        let request = json!({"view":view});
        assert_eq!(s.execute("render.result.view", &request).unwrap(), request);
    }
    assert_eq!(s.execute("render.result.close", &json!({})).unwrap(), json!({"view":"model"}));
    for request in
        [json!(null), json!({}), json!({"width":0,"height":1}), json!({"width":u64::MAX,"height":1}), json!({"width":1,"height":1,"extra":true})]
    {
        assert!(s.execute("render.output.size", &request).is_err());
    }
    for request in [json!(null), json!({"view":"unknown"}), json!({"view":"image","extra":true})] {
        assert!(s.execute("render.result.view", &request).is_err());
        // Session normalizes null parameters to an empty object for no-argument commands.
        if request.is_object() {
            assert!(s.execute("render.result.close", &request).is_err());
        }
    }
    assert!(std::sync::Arc::ptr_eq(&document, &s.doc));
    assert_eq!(before, (s.revision, s.undo.len(), s.evaluations));
}
