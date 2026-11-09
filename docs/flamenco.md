# Flamenco (farm) notes

GigaMux submits jobs to a stock Flamenco Manager/Worker. `GigaMux.js` authors `misc`/`exec` tasks that run the worker’s `ffmpeg`.

## Connector (Settings → Files)

| Action | What it does |
| --- | --- |
| **Find** | UPnP/SSDP discovery (`urn:flamenco:manager:0`), same idea as Flamenco workers. Manager needs `autodiscoverable` enabled. |
| **Deploy GigaMux…** | File browser: **Manager install only** (folder with `flamenco-manager.yaml`). Writes `scripts/GigaMux.js` plus `tools/gigamux-check.exe` and `tools/gigamux-check` (Linux). Releases ship both helpers under `helpers/` next to the app; from-source, prebuild into `flamenco/helpers/` — see that README. Linux `+x` on SMB: Samba `create mask` / `force create mode` of `06xx` strips execute on create; use something like `0775` so Windows deploys keep owner `+x` for Linux workers that `execve` the helper off the share. |
| **Worker check helper** | Absolute path workers use for `gigamux-check` (two-way braced). Deploy sets it to `{ManagerInstall}/tools/gigamux-check`, which **assumes the Manager install itself is on shared storage**. If Manager is local-only, browse/enter a shared path that has both Win + Linux helper binaries. |
| **Ensure tags** | `POST /api/v3/worker-mgt/tags` for missing `nvenc`, `qsv`, `vmaf`. Does not assign workers to tags; do that in Flamenco's worker UI. |
| **Flamenco** (main UI) | Builds ffmpeg argv locally and `POST /api/v3/jobs` (`type: GigaMux`). |

Manual Manager URL still works if SSDP is off or blocked.

## How Flamenco resolves ffmpeg

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
| --- | --- |
| `nvenc` | Opaque HW encode |
| `qsv` | VP9 QSV opaque |
| `vmaf` | Farm VMAF (later) |

Assign workers to those tags in Flamenco after probing. Submit with **Worker tag** set to the name (or UUID) when the job must stay on a tagged pool.

## Job type

- Script: [CODE0](../flamenco/GigaMux.js) (also embedded in the app for Deploy)
- Single encode: one `misc`/`exec` task (`encode_argvs_json` length 1, or legacy `ffmpeg_argv_json`)
- Chunked encode (sender option): `encode-c0`…`encode-cN` (video-only frame slices) → `concat` task (ffconcat join + mux source audio). Chunk files are named `{stem}.c000of004.{ext}` (Check already ignores these). The submitter writes the `.ffconcat` onto shared storage before POST so workers need no probe.

When GigaMux ships a newer bundled `GigaMux.js`, redeploy it to the Manager via **Settings → Files → Deploy GigaMux.js…**. The Manager keeps its own copy until you do this, and GigaMux will not be able to submit jobs.

### Two-way / mixed OS paths

Flamenco only auto-rewrites a setting when the **entire** string starts with the shared prefix (Blender `blendfile`-style). GigaMux stores paths inside `encode_argvs_json`, so the submitter braces them itself: `Z:\GigaMux\…` → `{shared_storage_path}/GigaMux/…`. Set **Settings → Files → Shared path prefix** to the same local root as the Manager two-way value (e.g. `Z:\` with Linux `/mnt/shared`, Audience All). Variable name defaults to `shared_storage_path`.

Farm `.ffconcat` lists use **basenames** (chunks next to the list) so Linux workers are not stuck with Windows absolute paths inside the file.

### Sender chunk options

| Control | Default | Behavior |
| --- | --- | --- |
| Chunk encode | off | N equal frame ranges (`2`–`32`), then concat/mux |
| Only chunk software | on | HW opaque paths stay single-task; software/alpha still chunk |

Skip if the final deliver already exists. Chunk encodes and mux use ffmpeg `-n` (never overwrite).

## Cancel

Workers poll ~10s, then kill the `exec` child. A few seconds of continued encode after Cancel is normal.

Upstream Flamenco currently has a bug where the Windows workers are not killed upon task/job cancel (Linux was usually fine). Windows workers need to be fixed upstream for this. Thus, GigaMux warns when working with Flamenco while the Manager is not in beta. This will be the remaining behavior until it's either fixed upstream, or until I wish to integreate a workaround that doesn't depend on an upstream fix, and/or that will work with Flamenco 3.9 and below.

## Limitations

- Local (non-Flamenco) encode has no chunking.
- QSV+libvpx alpha hybrid is refused for chunked farm jobs; use software alpha or opaque.
- No seam-scan on the farm (Python-only locally today).
- Farm VMAF only after workers with `libvmaf` are tagged and confirmed.
- SSDP Find does not configure Manager; URL is written to prefs when a Manager answers `/api/v3/version`.
- Whether or not cancellation works depends on Worker build and (for ≤3.9) GigaMux’s legacy circumvention path.
