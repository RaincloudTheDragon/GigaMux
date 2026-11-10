# Flamenco job type

Prefer **Settings → Files → Deploy GigaMux…** in the app.

Pick the **Manager install folder** (must contain `flamenco-manager.yaml`). Deploy writes:

- `scripts/GigaMux.js`
- `tools/gigamux-check.exe` (Windows)
- `tools/gigamux-check` (Linux)

Release builds ship both helpers next to `GigaMux.exe` under `helpers/` — Deploy finds them there. From-source builds: place binaries in [`helpers/`](helpers/) first (see that README). Deploy defaults **Worker check helper** to `{ManagerInstall}/tools/gigamux-check`, which assumes the Manager install is on **shared storage**. Override that path in Settings → Files if helpers live elsewhere.

## Cancel

Upstream Flamenco currently has a bug where the Windows workers are not killed upon task/job cancel (Linux was usually fine). Windows workers need to be fixed upstream for this. Thus, GigaMux warns when working with Flamenco while the Manager is not in beta. This will be the remaining behavior until it's either fixed upstream, or until I wish to integreate a workaround that doesn't depend on an upstream fix, and/or that will work with Flamenco 3.9 and below.

GigaMux sets job `cancel_exec` from Manager `/api/v3/version`:

- **≤ 3.9** → `legacy_stub` (warn; helper not shipped yet — still direct exec)
- **3.10 beta** / ≥3.10 → `direct` (Assumes worker will kill)

Details (including the in-app **Farm** session log panel): [docs/flamenco.md](../docs/flamenco.md).
