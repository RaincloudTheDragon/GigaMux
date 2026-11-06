# Flamenco job type: GigaMux

Prefer **Settings → Files → Deploy GigaMux.js…** in the app (picks Manager install or `scripts/`).

Manual: copy [`GigaMux.js`](GigaMux.js) into the Manager’s `scripts/` folder. Flamenco 3.7+ hot-loads; older builds may need a restart.

Then **Find** or set Manager URL, **Ensure tags** if you want `nvenc` / `qsv` / `vmaf`, and **Flamenco** / **Send to Flamenco** on an input.

## Cancel

Upstream Flamenco currently has a bug where the Windows workers are not killed upon task/job cancel (Linux was usually fine). Windows workers need to be fixed upstream for this. Thus, GigaMux warns when working with Flamenco while the Manager is not in beta. This will be the remaining behavior until it's either fixed upstream, or until I wish to integreate a workaround that doesn't depend on an upstream fix, and/or that will work with Flamenco 3.9 and below.

GigaMux sets job `cancel_exec` from Manager `/api/v3/version`:

- **≤ 3.9** → `legacy_stub` (warn; helper not shipped yet — still direct exec)
- **3.10 beta** / ≥3.10 → `direct` (Assumes worker will kill)

Details and log reading: [docs/flamenco.md](../docs/flamenco.md).
