# Flamenco (farm) notes

GigaMux submits jobs to a stock Flamenco Manager/Worker. `GigaMux.js` authors `misc`/`exec` tasks that run the worker’s `ffmpeg`.

## Connector (Settings → Files)

| Action | What it does |
|--------|----------------|
| **Find** | UPnP/SSDP discovery (`urn:flamenco:manager:0`), same idea as Flamenco workers. Manager needs `autodiscoverable` enabled. |
| **Deploy GigaMux.js…** | File browser: pick Manager install (folder with `flamenco-manager.yaml`) or `scripts/` directly; writes bundled `GigaMux.js`. |
| **Ensure tags** | `POST /api/v3/worker-mgt/tags` for missing `nvenc`, `qsv`, `vmaf`. Does not assign workers to tags; do that in Flamenco's worker UI. |
| **Flamenco** (main UI) | Builds ffmpeg argv locally and `POST /api/v3/jobs` (`type: GigaMux`). |

Manual Manager URL still works if SSDP is off or blocked.

## How Flamenco resolves `ffmpeg`

1. Prefer a binary next to the worker executable under `tools/` (`ffmpeg.exe`, `ffmpeg-windows.exe`, ...). See Flamenco `internal/find_ffmpeg`.
2. Otherwise `$PATH` (`exec.LookPath("ffmpeg")`).

`frames-to-video` uses that finder when `exe` is literally `ffmpeg` / `ffmpeg.exe`. GigaMux uses `misc`/`exec`, so the submitter can pass an explicit worker ffmpeg path or leave `ffmpeg` for PATH/`tools`.

## Probe workers before HW / VMAF jobs

Capabilities differ per worker (PATH vs Flamenco `tools/ffmpeg`, NVENC gen, QSV, `libvmaf`). On each worker class you care about:

```bat
ffmpeg -hide_banner -encoders
ffmpeg -hide_banner -filters
```

Check for the encoders/filters your jobs need before tagging or submitting.

## Worker tags

Flamenco: jobs with a `worker_tag` only run on workers in that tag; tagless jobs are available to all workers; tagless workers only take tagless jobs.

GigaMux **Ensure tags** creates:

| Name | Intended use |
|------|----------------|
| `nvenc` | Opaque HW encode |
| `qsv` | VP9 QSV opaque |
| `vmaf` | Farm VMAF (later) |

Assign workers to those tags in Flamenco after probing. Submit with **Worker tag** set to the name (or UUID) when the job must stay on a tagged pool.

## Job type

- Script: [`flamenco/GigaMux.js`](../flamenco/GigaMux.js) (also embedded in the app for Deploy)
- v0 graph: one `misc` task, one `exec` of `ffmpeg` + argv JSON from the UI

## Limitations (v0)

- No chunk/concat graph yet (local encode also has no chunking on main).
- QSV+libvpx alpha hybrid is multi-ffmpeg locally. Flamenco submit refuses that combo until a multi-task graph exists; use software alpha or opaque.
- Farm VMAF only after workers with `libvmaf` are tagged and confirmed.
- SSDP Find does not configure Manager; URL is written to prefs when a Manager answers `/api/v3/version`.
