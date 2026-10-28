# Process plug-ins

A process plug-in is a program that SolidCraft starts and talks to in JSON lines over the
program's stdin and stdout. It can put a button on the application bar, describe a panel that
SolidCraft draws, run any control-channel method (the same ones as `docs/control-protocol.md`),
and hear the panel's clicks and edits. Everything else (network, files) the program does itself.

The host is `crates/ui-egui/src/bh_plugins.rs`. It is desktop only; the browser build has none.

## Installing

A plug-in is a folder named after its id, holding:

| File | What |
|---|---|
| `plugin.json` | The upstream manifest: `{"id", "name", "version", "description"}`. It lists the plug-in under Preferences › Plugins, where it can be turned off. |
| `bh.json` | `{"run": "<program>", "args": [...], "button": {"label", "icon", "tooltip"}}`. `run` is relative to the folder and must stay inside it (`.exe` is added on Windows). `icon` is a name from the app's own icon set (`folder`, `open`, …). |
| the program | Anything executable. |

SolidCraft looks in these folders, in order; the first folder with an id wins:

1. the user's plug-ins folder (`~/.local/share/solidcraft/plugins` on Linux, `%LOCALAPPDATA%\SolidCraft\plugins` on Windows),
2. the plug-ins packaged with the app: `<exe>/../share/solidcraft/plugins` (`packaging/linux/package.sh` copies `BH_PLUGINS_DIR` there),
3. `SOLIDCRAFT_BH_PLUGINS` (development).

The program starts with the app, in its folder, with these variables:

| Variable | Value |
|---|---|
| `SOLIDCRAFT_PLUGIN_ID` | its id |
| `SOLIDCRAFT_PLUGIN_DIR` | its folder (may be read-only, e.g. inside an AppImage) |
| `SOLIDCRAFT_PLUGIN_DATA` | a writable folder of its own: `<data>/plugin-data/<id>` |
| `SOLIDCRAFT_VERSION` | the app's version |

It must exit when its stdin closes: that is how the app says it quit. Its stderr goes to the app's.
If it stops, its button shows ⚠ and a click starts it again.

## Messages

Every line is one JSON object. A message from the program may carry several of these keys.

### Program → SolidCraft

| Key | Value | Effect |
|---|---|---|
| `button` | `{"label"?, "icon"?, "tooltip"?}` | Changes the application-bar button. |
| `panel` | `{"title", "width", "body": [elements]}` | Replaces the panel. It is drawn while open. |
| `open` | `true` / `false` | Opens or closes the panel. |
| `status` | `{"text", "error"?}` | Shows a line in the app's status bar. |
| `request` | `{"id", "method", "params"}` | Runs a control-channel method (e.g. `engine.execute` with `{"command", "params"}`, `document.inspect`). The answer comes back as `response` with the same `id`. Requests wait their turn like the control channel's (a file loading, a save). At most 64 wait at once. |

### SolidCraft → program

| Key | Value |
|---|---|
| `event` | `{"type": "start", "version", "language"}` once, first. |
| | `{"type": "open"}` / `{"type": "close"}` when the user opens or closes the panel. |
| | `{"type": "click", "id"}` for a button. |
| | `{"type": "input", "id", "value"}` on every edit of a text field, `{"type": "submit", "id", "value"}` on Enter. |
| | `{"type": "card", "list", "id", "action"}`: a click on a card is `action: "open"`, on one of its actions that action's id. |
| `response` | The control-channel reply (`{"ok": true, "result"}` or `{"ok": false, "error"}`) with the request's `id`. |

## Panel elements

`body` is a list of elements, each `{"type": ...}`:

| Type | Fields |
|---|---|
| `row`, `column` | `items`: elements side by side (wrapping) or stacked. |
| `heading` | `text` |
| `text` | `text`, `small`?, `strong`?, `color`? (`dim`, `error`, `warning`, `accent`) |
| `button` | `id`, `label`, `primary`?, `small`?, `enabled`?, `tooltip`? |
| `input` | `id`, `value`, `hint`?, `width`?, `reset`?: the field keeps what the user types; to set its text, send `value` with a new `reset` number. |
| `separator`, `space` | `space` has `size`. |
| `progress` | `value` (0–1), `text`?, `width`? |
| `cards` | `id`, `min_width`?, `items`: `{"id", "title", "subtitle", "image", "badge"?, "selected"?, "tooltip"?, "actions": [{"id", "label", "danger"?, "always"?}]}`. `image` is a PNG or JPEG file path, decoded once per path: write a new file name for a new picture. Actions show on hover, or always with `always`. |

## Example

```sh
#!/bin/sh
# A plug-in that makes a box when its button is pressed.
read start
echo '{"panel":{"title":"Box","width":300,"body":[{"type":"button","id":"make","label":"Make a box","primary":true}]}}'
while read line; do
  case "$line" in
    *'"click"'*) echo '{"request":{"id":1,"method":"engine.execute","params":{"command":"solid.box","params":{"length":10,"width":10,"height":10}}}}' ;;
  esac
done
```
