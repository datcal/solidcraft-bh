# solidcraft-bh

[SolidCraft](https://github.com/tangency-labs/solidcraft) releases with **process plug-ins**: programs
the app starts that add a button and a panel and run commands ([plugins/PROTOCOL.md](plugins/PROTOCOL.md)).
The Linux AppImage built here carries the [designs](plugins/designs/README.md) plug-in, and its
**Update now** installs the next build from this repository.

## Branches and versions

- `bh` (default): an upstream release tag with our changes merged on top. `bh/UPSTREAM` names that tag.
- `sync/vX.Y.Z`: a pull request that merges upstream `vX.Y.Z` into `bh`.
- Remote `upstream` = `https://github.com/tangency-labs/solidcraft.git`. This repository never builds upstream `main`.
- Releases are `v<upstream version>-bh.<CI run number>`, e.g. `v0.25.1-bh.7`. The app treats `-bh.N` as a stable release.

## What we change in upstream code

Keep it this small. Every change is marked with a `bh:` comment.

| File | Change |
|---|---|
| `crates/ui-egui/src/bh_plugins.rs` | New: the plug-in host. |
| `crates/ui-egui/src/lib.rs` | `pub mod bh_plugins;`; `bh_plugins::pump(self, ctx)` in `SolveApp::logic` just before `poll_control_jobs`; `bh_plugins::show(self, ui.ctx())` at the end of `SolveApp::ui`. |
| `crates/ui-egui/src/toolbar.rs` | In `app_bar`, right after the Update pill: `bh_plugins::app_bar(...)`, and `tabs_end` stops before its buttons. |
| `crates/ui-egui/src/update.rs` | `SOLIDCRAFT_BH_REPO` (set by our CI at build time) points update checks at this repository; `is_newer` accepts `-bh.N`. |
| `packaging/linux/package.sh` | Copies `BH_PLUGINS_DIR` to `usr/share/solidcraft/plugins`. |
| `.github/workflows/*` | Upstream's workflows are deleted: this repository has none of their secrets, and they must not run on its schedules, pull requests or releases. Only `bh-*.yml` stays. |
| `bh/`, `.github/workflows/bh-*.yml` | Ours. |

`git diff "$(cat bh/UPSTREAM)" bh -- crates packaging` shows exactly this.

## Syncing a new upstream release

A scheduled agent runs this. A person can too.

1. `git fetch upstream --tags`. The newest release is
   `git tag -l 'v*' --sort=-v:refname | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -1`.
   Nightlies and release candidates (`-…`) are skipped.
2. **Open sync PR:** if a `sync/v…` pull request is already open, check its CI.
   - CI failed: fix the cause on that branch and push.
   - CI passed or is still running: leave it; the owner merges it.
   - Either way, stop here.
3. **Up to date:** if the newest release equals `bh/UPSTREAM`, stop.
4. **Start a sync:**
   - `git switch -c sync/vX.Y.Z origin/bh`
   - `git merge --no-ff vX.Y.Z -m "Merge upstream vX.Y.Z"`
   - Resolve conflicts: keep upstream's code and re-apply our hooks as in the table above, minimal and marked `bh:`.
   - Delete every workflow the merge brings back or adds: `git rm` anything in `.github/workflows/` not named `bh-*` (a change upstream made to one of them shows as a modify/delete conflict; resolve it by deleting).
   - If upstream now has its own way to run plug-ins or panels, keep ours working and describe the overlap in the pull request instead of redesigning.
5. **Update the marker:** write `vX.Y.Z` to `bh/UPSTREAM` and commit.
6. **Test:**
   - `cargo test -p solidcraft-ui-egui --lib bh_plugins`
   - `cargo test -p solidcraft-ui-egui --lib update`
   - `cargo test --manifest-path bh/plugins/designs/Cargo.toml`

   Fix what fails. If the environment cannot build the workspace in time, say so in the pull request and let CI decide.
7. **Open the pull request:**
   - Push `sync/vX.Y.Z` and open a pull request into `bh` titled `Sync with upstream vX.Y.Z`.
   - In the body, list the conflicts, what you changed to resolve them, and the test results.
   - Never push to `bh`, never merge, never tag or release: the owner approves every merge.

Never add a domain name, address, token or anything about the owner's servers to this repository. The designs plug-in reads those from the user's own settings.

## CI: `.github/workflows/bh-build.yml`

- **Pull request into `bh`:** runs the tests above (release profile, so they share the build cache), builds the AppImage with the plug-in, and uploads it as an artifact to try before merging.
- **Push to `bh`** (a merged pull request) publishes the release.
  - When the merge commit's tree is the pull request's (it was up to date with `bh`), the pull request's tested AppImage is released as it is, under its own version, within a minute. Anything else is built again.
  - Assets: `solidcraft-<version>-linux-x86_64.AppImage`, its `.zsync`, the `solidcraft-linux-x86_64.AppImage` name the app's updater downloads, and `SHA256SUMS.txt`.
  - Only `bh` saves the Rust cache (a pull request's cache is visible to itself alone). After a release that reused a build, a `cache` job builds once more so the next pull request starts warm.
  - It also turns off any other workflow GitHub still knows about, in case one slipped back in.

## Installing

Download `solidcraft-linux-x86_64.AppImage` from the latest release, make it executable and run it.
From then on **Update now** in the app installs the next release from here.
