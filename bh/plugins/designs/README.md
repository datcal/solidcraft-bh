# designs

A process plug-in ([../PROTOCOL.md](../PROTOCOL.md)) that keeps designs on your own HTTPS server
behind Cloudflare Access, with the same store the self-hosted web app uses.

- **Designs** on the application bar opens a panel of cards: each design's picture, name, age and size.
  A click opens it; Delete removes it (its history stays on the server).
- An opened design lives in `<data>/open/<name>.sc`. Every save of that file (the panel's Save,
  the app's Save, Ctrl+S) is uploaded, with a copy in the store's history.
- Save on a design that is not from the store asks for a name and saves it there.
- Settings asks for the store's address and a Cloudflare Access service token. They are kept in
  `<data>/settings.json` (mode 600) on this computer only, never in this repository.

`<data>` is `SOLIDCRAFT_PLUGIN_DATA`: `~/.local/share/solidcraft/plugin-data/designs` on Linux.

## Store API

Every request carries `CF-Access-Client-Id` and `CF-Access-Client-Secret`. A redirect, 401 or
403 means the token was refused.

| Request | What |
|---|---|
| `GET /designs/` | The listing as JSON (nginx `autoindex_format json`): `[{"name", "type": "file", "mtime": "<RFC 2822>", "size"}]`. Dot folders are left out. |
| `GET /designs/<name>.sc` | A design. |
| `PUT /designs/<name>.sc` | Saves a design. |
| `PUT /designs/.history/<name>/<YYYYMMDD-HHMMSS>.sc` | A copy kept after each save. |
| `DELETE /designs/<name>.sc` | Deletes a design. |
| `GET /designs/.previews/<name>.png` | Its picture, drawn by the server after a save (404 until then). |

Names are ASCII letters, digits, `-` and `_`.

## Building

```sh
cargo build --release --manifest-path bh/plugins/designs/Cargo.toml
```

The CI packages `plugin.json`, `bh.json` and the program into the AppImage's plug-ins folder.
