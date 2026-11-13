# Flamenco (farm) notes

GigaMux submits jobs to a stock Flamenco Manager/Worker. `GigaMux.js` authors `exec` tasks with **capability task types** (`nvenc`, `cpu`, …). Encode/VMAF jobs are **tagless**; only Probe Farm uses temporary `gm-…` pin tags.

## Connector (Settings → Files)

| Action | What it does |
| --- | --- |
| **Find** | UPnP/SSDP discovery (`urn:flamenco:manager:0`), same idea as Flamenco workers. Manager needs `autodiscoverable` enabled. |
| **Deploy GigaMux…** | File browser: **Manager install only** (folder with `flamenco-manager.yaml`). Writes `scripts/GigaMux.js` plus `tools/gigamux-check.exe` and `tools/gigamux-check` (Linux). Releases ship both helpers under `helpers/` next to the app; from-source, prebuild into `flamenco/helpers/` — see that README. Linux `+x` on SMB: Samba `create mask` / `force create mode` of `06xx` strips execute on create; use something like `0775` so Windows deploys keep owner `+x` for Linux workers that `execve` the helper off the share. |
| **Worker check helper** | Absolute path workers use for `gigamux-check` (two-way braced). Deploy sets it to `{ManagerInstall}/tools/gigamux-check`, which **assumes the Manager install itself is on shared storage**. If Manager is local-only, browse/enter a shared path that has both Win + Linux helper binaries. |
| **Probe Farm** | One button: probe every non-offline worker (`gm-…` pin, then restore+delete), merge host JSON into prefs, then open **task_types** recommendations and **Export** `{worker}_flamenco-worker.yaml` files. Right-click → clear prefs inventory only. |
| **Worker YAMLs…** | Re-open recommendations/export from existing share JSON (no new probe jobs). |
| **Flamenco** (main UI) | Builds ffmpeg argv locally and `POST /api/v3/jobs` (`type: GigaMux`). Uses the Input selection when that side is active; with no Input selection, sends **every** listed input (one job each). |
| **Farm** (Flamenco mark button) | Separate OS window for jobs submitted this GigaMux run: status + colored Manager log tail. Opens after a successful submit; closing the window does not cancel jobs. |

Manual Manager URL still works if SSDP is off or blocked.

## Farm panel (session logs)

Tracks job ids returned from submits in **this GigaMux session** only (restart clears the list). While the panel is open it polls Manager every few seconds:

- `GET /api/v3/jobs/{id}` + `…/tasks` for non-terminal jobs
- Full task logs via `GET /api/v3/tasks/{id}/log` → `/job-files/…` (not the 2 KiB `logtail`); all started tasks are concatenated so later tasks do not wipe earlier stderr

Freshness still waits on worker log batching (~10 KB or ~30 s). GigaMux cannot make logs faster without a worker change. **Open in Manager** opens the Manager web root (SPA has no stable per-job deep link).

## How Flamenco resolves ffmpeg

1. Prefer a binary next to the worker executable under `tools/` (`ffmpeg.exe`, `ffmpeg-windows.exe`, ...). See Flamenco `internal/find_ffmpeg`.
2. Otherwise `$PATH` (`exec.LookPath("ffmpeg")`).

`frames-to-video` uses that finder when `exe` is literally `ffmpeg` / `ffmpeg.exe`. GigaMux uses `exec`, so the submitter can pass an explicit worker ffmpeg path or leave `ffmpeg` for PATH/`tools`.

## Probe workers before HW / VMAF jobs

Capabilities differ per worker (PATH vs Flamenco `tools/ffmpeg`, NVENC gen, QSV, `libvmaf`). On each worker class you care about:

```bat
ffmpeg -hide_banner -encoders
ffmpeg -hide_banner -filters
```

Check for the encoders/filters your jobs need before configuring `task_types` or submitting.

## Task types

Flamenco workers declare accepted **task types** in `flamenco-worker.yaml` (`task_types: […]`). The scheduler only assigns a task if that type is listed for the worker. GigaMux encode/VMAF jobs are **tagless**; routing is entirely via these types.

There is **no Manager API** today to push `task_types` (worker-local update API is still WIP: [Flamenco #104469](https://projects.blender.org/studio/flamenco/pulls/104469)). Probe Farm therefore **recommends** types and exports YAML for manual delivery.

| Task type | Used for |
| --- | --- |
| `misc` | Probe Farm pin jobs only (always include until automagic apply exists) |
| `nvenc` | Opaque NVENC encode (any generation) |
| `nvenc-{gen}` | Opaque NVENC pinned to a generation (`nvenc-turing`, `nvenc-ampere`, `nvenc-ada`, …) |
| `qsv` | Opaque VP9 QSV encode |
| `vmaf` | Attached / Separate / batch VMAF (`ensure-vmaf`) |
| `cpu` | Software encode, mux/remux, final-check/cleanup |

**Encode task type** comes from **Output → Targets** (NVENC generation under Use hardware):

| Slot HW path | Encode task type |
| --- | --- |
| Opaque NVENC, generation **Any** | `nvenc` |
| Opaque NVENC, pinned gen | `nvenc-{gen}` (e.g. `nvenc-turing`) |
| Opaque VP9 HW | `qsv` |
| Alpha / software | `cpu` |

Chunked jobs: `encode-c*` use the encode type above; `mux` / `remux` / `final-check` use `cpu`; trailing VMAF uses `vmaf`.

Test mode: one Flamenco job per input × active slot (`test1_*`…), each with that slot’s encode task type — so VMAF can compare gens across the farm. Normal mode: one job per input from slot1. Farm argv does not forward the submitter’s local `-gpu` index. QSV generation picker is deferred.

### Worker YAML export

After Probe Farm (or **Worker YAMLs…**):

1. Dialog lists each Manager worker matched by **hostname ↔ worker name**, with recommended `task_types` (always includes `misc`).
2. **Export YAMLs…** writes `{workername}_flamenco-worker.yaml` (manager URL, `task_types`, `restart_exit_code: 47`).
3. On each machine: copy/rename to `flamenco-worker.yaml` in that worker’s **working directory**, then restart Flamenco Worker.

**Do not** run multiple workers with the same working directory / shared `flamenco-worker.yaml`. A shared **binary** on the share is fine; config is cwd-relative until Flamenco moves to a well-known path (e.g. `~/.local/flamenco/` — see [#104469](https://projects.blender.org/studio/flamenco/pulls/104469) future work).

### QSV hybrid (future)

Local QSV+libvpx alpha hybrid stays one machine. On the farm, task types make a **single-job** graph feasible later (`qsv` color + `cpu` alpha chunks + `cpu` remux) without job-level tags. Not implemented yet; farm hybrid remains refused (use software alpha or opaque).

### Probe Farm pins only

Probe still uses temporary Manager **`gm-…` pin tags** so each worker gets exactly one probe job. Pins are restored/deleted after the job. Capability routing no longer uses settags.

## Job type

Script: [GigaMux.js](../flamenco/GigaMux.js) (bundled for Deploy). Redeploy after GigaMux updates; the Manager keeps its own copy until you do. Fleet workers must install exported `task_types` before encode routing works.

- **Single:** `encode` → `final-check` (`cpu`) → optional `vmaf`
- **Chunked:** `encode-c*` (`ensure-chunk`) → `mux` / `remux` (`cpu`) → `final-check` → `cleanup` → optional `vmaf`
- **VMAF-only:** `vmaf` / `vmaf-i` from `vmaf_pairs_json` (no encode); tagless job, task_type `vmaf`
- **Farm-probe:** `misc` + `gm-…` pin tag

Flamenco soft-fail retries only that task, so `mux` and `remux` stay separate (a remux retry must not re-join chunks). Chunk healthcheck runs in the same task as the encode; `final-check` is separate and may fail the job. Helpers: [flamenco/helpers/README.md](../flamenco/helpers/README.md).

### Farm VMAF

**Send to Flamenco** dialog → **Farm VMAF** (default Off; not persisted):

| Mode | Behavior |
| --- | --- |
| Off | No farm VMAF tasks |
| Attached | Trailing `ensure-vmaf` on the encode job (task_type `vmaf` — those workers need `libvmaf`) |
| Separate | After encode **completed**, submit a VMAF-only job (task_type `vmaf`) |

Right-click the main **VMAF** button to submit a VMAF-only job for unscored completed finals (`vmaf`).

`gigamux-check ensure-vmaf` writes `{deliver}.vmaf.json` next to the output and **skips** ffmpeg when that sidecar already has a score (soft-fail/retry never redoes heavy scoring). GigaMux imports sidecars into the preview cache on list refresh / farm poll; local idle VMAF treats a valid sidecar as already scored. Redeploy `GigaMux.js` and update worker `gigamux-check` after this change.

`mux` / `remux` run `setts=TS-STARTPTS` so A/V start times land at zero; residual skew on Check/farm final is a Low WARN only (does not hard-fail or block cleanup). Local MP4 encode and farm MP4 remux use `-movflags +use_metadata_tags` so `GigaMux_version` / `GigaMux_*` tags show in ffprobe (Lavf may still overwrite the standard `encoder` tag).

`mux` stages through Matroska so multi-audio MP4 copy does not hit mid-file `-22`; there is no `.vcat` intermediate (join+audio is one pass). On a bad final, requeue `remux` + `final-check` (and `mux` only if `.vmux.mkv` is missing/bad).

Jobs expect `gigamux-check` beside the Manager on shared storage; otherwise set **Worker check helper** to a path with both Win + Linux binaries.

### Two-way / mixed OS paths

Flamenco only auto-rewrites a setting when the **entire** string starts with the shared prefix (Blender `blendfile`-style). GigaMux stores paths inside `encode_argvs_json`, so the submitter braces them itself: `Z:\GigaMux\…` → `{shared_storage_path}/GigaMux/…`. Set **Settings → Files → Shared path prefix** to the same local root as the Manager two-way value (e.g. `Z:\` with Linux `/mnt/shared`, Audience All). Variable name defaults to `shared_storage_path`.

Farm `.ffconcat` lists use **basenames** (chunks next to the list) so Linux workers are not stuck with Windows absolute paths inside the file.

### Sender chunk options

| Control | Default | Behavior |
| --- | --- | --- |
| Chunk encode | off | N equal frame ranges (`2`–`32`), then mux/remux |
| Only chunk software | on | HW opaque paths stay single-task; software/alpha still chunk |

Skip if the final deliver already exists (submitter). Chunk argv is submitted as `-n`; `ensure-chunk` / `ensure-out` only force `-y` when rebuilding a bad/missing file. Batch submit shows a progress bar (and taskbar fill) and keeps the UI responsive; Esc cancels remaining jobs after the in-flight POST.

## Cancel

Workers poll ~10s, then kill the `exec` child. A few seconds of continued encode after Cancel is normal.

Upstream Flamenco currently has a bug where the Windows workers are not killed upon task/job cancel (Linux was usually fine). Windows workers need to be fixed upstream for this. Thus, GigaMux warns when working with Flamenco while the Manager is not in beta. This will be the remaining behavior until it's either fixed upstream, or until I wish to integreate a workaround that doesn't depend on an upstream fix, and/or that will work with Flamenco 3.9 and below.

## Limitations

- Local (non-Flamenco) encode has no chunking.
- QSV+libvpx alpha hybrid is refused on the farm until a single-job task-type graph ships; use software alpha or opaque.
- No Manager write API for worker `task_types` yet; Probe Farm only recommends/exports YAML ([#104469](https://projects.blender.org/studio/flamenco/pulls/104469)). Automagic apply can return once Manager can reach each worker’s API.
- Workers must not share one [cwd](https://en.wikipedia.org/wiki/Working_directory) / `flamenco-worker.yaml` (shared binary OK).
- Farm probe uses one job per worker via temporary `gm-…` pin tags; pins are deleted after each job. Batch end merges JSON and opens YAML recommendations (hostname ↔ Manager worker name).
- Encode task types are derived from Targets NVENC generation (not a global Send combo). Test mode submits one farm job per slot.
- No seam-scan on the farm (Python-only locally today).
- Local VMAF (manual or idle auto) scores leftover `*.cNNNofMMM.*` chunks against the source frame range implied by the filename; Check/`check_chunk_file` must pass first (incomplete mid-farm chunks are skipped / blocked like bad finals).
- Attached farm VMAF requires `libvmaf` on workers that accept task_type `vmaf`; Separate/batch use `vmaf` only.
- SSDP Find does not configure Manager; URL is written to prefs when a Manager answers `/api/v3/version`.
- Whether or not cancellation works depends on Worker build and (for ≤3.9) GigaMux’s legacy circumvention path.
