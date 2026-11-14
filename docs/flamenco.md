# Flamenco (farm) notes

GigaMux submits jobs to a stock Flamenco Manager/Worker. `GigaMux.js` authors `exec` tasks with **capability task types** (`nvenc`, `cpu`, …). Encode/VMAF jobs are **tagless**; only Probe Farm uses temporary `gm-…` pin tags.

## Connector (Settings → Files)

| Action | What it does |
| --- | --- |
| **Find** | UPnP/SSDP discovery (`urn:flamenco:manager:0`), same idea as Flamenco workers. Manager needs `autodiscoverable` enabled. |
| **Deploy GigaMux…** | File browser: **Manager install only** (folder with `flamenco-manager.yaml`). Writes `scripts/GigaMux.js` plus `tools/gigamux-check.exe` and `tools/gigamux-check` (Linux). Releases ship both helpers under `helpers/` next to the app; from-source, prebuild into `flamenco/helpers/` — see that README. Linux `+x` on SMB: Samba `create mask` / `force create mode` of `06xx` strips execute on create; use something like `0775` so Windows deploys keep owner `+x` for Linux workers that `execve` the helper off the share. |
| **Worker check helper** | Absolute path workers use for `gigamux-check` (two-way braced). Deploy sets it to `{ManagerInstall}/tools/gigamux-check`, which **assumes the Manager install itself is on shared storage**. If Manager is local-only, browse/enter a shared path that has both Win + Linux helper binaries. |
| **Probe Farm** | One button: probe every non-offline worker (`gm-…` pin, then restore+delete), merge host JSON into prefs, open **task_types** recommendations / **Export** YAMLs, then **delete** `output/gigamux-farm-probe/` (reports stay in prefs). Right-click → clear prefs inventory only. |
| **Worker YAMLs…** | Re-open recommendations/export from prefs host reports (no new probe jobs; refreshes from share JSON if that folder still exists). |
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

**Farm VMAF:** `gigamux-check ensure-vmaf` / `ensure-vmaf-part` call `resolve_ffmpeg_for_vmaf`: if the job’s `ffmpeg` has no `libvmaf`, they fall back to Deploy siblings next to the helper (`tools/ffmpeg-linux-amd64`, `tools/ffmpeg-windows-amd64.exe`). PATH ffmpeg on many Linux distros lacks libvmaf; keep those full builds in Manager `tools/`.

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
| `qsv` | Opaque VP9 QSV encode; hybrid color IVF |
| `vmaf` | Attached / Separate / batch VMAF (`ensure-vmaf`) |
| `cpu` | Software encode, mux/remux, final-check/cleanup |

**Encode task type** comes from **Output → Targets** (NVENC generation under Use hardware), unless **Settings → Files → NVENC generation isolation (farm)** is off (then all NVENC → coarse `nvenc`):

| Slot HW path | Encode task type |
| --- | --- |
| Opaque NVENC, generation **Any** | `nvenc` |
| Opaque NVENC, pinned gen | `nvenc-{gen}` when isolation is on; else `nvenc` |
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

### QSV hybrid (α VP9 / `.webm` + Use hardware)

Farm submits a **single-job** multi-task-type graph. Intermediates sit **flat in the output dir** (same `{stem}.cNNNofMMM.*` layout as opaque chunking:

1. `encode-color` (`qsv`) → `{stem}.color.ivf` — **chunkless by default** (one QSV worker assumed)
2. `encode-alpha` (`cpu`) → `{stem}.alpha.ivf`, or `{stem}.cNNNofMMM.ivf` when **Chunk encode** is on
3. optional `concat-color` / `concat-alpha` (`cpu`) when those planes were chunked → `{stem}.color.ivf` / `{stem}.alpha.ivf`
4. `remux-alpha` (`cpu`) — `gigamux-check mux-alpha-webm` → `{stem}.vmux.webm` (Rust BlockAdditional, not ffmpeg like the others)
5. `audio` (`cpu`) — ffmpeg mux onto the deliver WebM
6. `final-check` / `cleanup` / optional `vmaf` as usual

Alpha IVF chunks use the deliver stem so leftover files get the same chunk VMAF / Check treatment as opaque `{stem}.cNNNofMMM.webm`. Optional multi-QSV color chunks are `{stem}.color.cNNNofMMM.ivf`.

`ensure-chunk` / IVF concat / hybrid `audio` expects use **video-stream** duration (not container/format). Frame-count is checked too so stale IVF chunks from an earlier split cannot pass reuse. Hybrid `audio` mux uses `-shortest` so trailing source audio does not pad format duration past the VP9 plane, and re-stamps source `color_range` / matrix (Lavf `-c:v copy` would otherwise drop Matroska Colour → ffprobe `tv`). Submit clears flat hybrid temps (and any legacy `{stem}.hybrid/`) before writing new lists. Send dialog → **Chunk QSV color (multi-QSV)** (default off) splits the color IVF when Chunk encode is on.

### Probe Farm pins only

Probe still uses temporary Manager **`gm-…` pin tags** so each worker gets exactly one probe job. Pins are restored/deleted after the job. Capability routing no longer uses settags.

## Job type

Script: [GigaMux.js](../flamenco/GigaMux.js) (bundled for Deploy). Redeploy after GigaMux updates; the Manager keeps its own copy until you do. Fleet workers must install exported `task_types` before encode routing works.

- **Single:** `encode` → `final-check` (`cpu`) → optional `vmaf`
- **Chunked:** `encode-c*` (`ensure-chunk`) → `mux` / `remux` (`cpu`) → `final-check` → `cleanup` → optional `vmaf`
- **Hybrid:** `encode-color` (`qsv`) + `encode-alpha` (`cpu`) → `remux-alpha` → `audio` → `final-check` → `cleanup` → optional `vmaf`
- **VMAF-only:** `vmaf` / `vmaf-p*` + `vmaf-merge` from `vmaf_pairs_json` (no encode); tagless job, task_type `vmaf`
- **Farm-probe:** `misc` + `gm-…` pin tag

Flamenco soft-fail retries only that task, so `mux` and `remux` stay separate (a remux retry must not re-join chunks). Chunk healthcheck runs in the same task as the encode; `final-check` is separate and may fail the job. Helpers: [flamenco/helpers/README.md](../flamenco/helpers/README.md).

### Farm VMAF

**Send to Flamenco** dialog → **Farm VMAF** (default Off; not persisted):

| Mode | Behavior |
| --- | --- |
| Off | No farm VMAF tasks |
| Attached | Trailing VMAF on the encode job (task_type `vmaf` — those workers need `libvmaf`) |
| Separate | After encode **completed**, submit a VMAF-only job (task_type `vmaf`) |

If the deliver **already exists**, encode is skipped; with Attached or Separate, GigaMux submits a **VMAF-only** job for that pair instead (unless `{deliver}.vmaf.json` already has a score).

Right-click the main **VMAF** button to submit a VMAF-only job for unscored completed finals (`vmaf`).

**Temporal parts:** long delivers are split into N parallel time slices using **Settings → Files → Flamenco chunks** (`2..=32`), even when encode chunking is off. Each part trims the **same** frame range on reference and distorted (`ensure-vmaf-part` → `{deliver}.vmaf.pIIIofNNN.json`), then `merge-vmaf` writes a frame-weighted mean into `{deliver}.vmaf.json` and deletes the part sidecars. Clips too short for ~3s/part stay a single `ensure-vmaf` (unchanged). Soft-fail retry of one `vmaf-p*` does not redo other parts. Local leftover encode-chunk VMAF (`*.cNNNofMMM.*`) is unchanged (reference-only trim) and is **not** farm-scored.

`gigamux-check ensure-vmaf` / `ensure-vmaf-part` write sidecars and **skip** ffmpeg when that sidecar already has a score (soft-fail/retry never redoes heavy scoring). Local VMAF writes the same final sidecar and skips pairs that already have one (manual / idle / single-file); farm **Attached** omits the trailing task when the sidecar exists, and Separate / right-click batch already skip scored finals. Alpha WebMs decode via libvpx; before `libvmaf`, each side is probed for **straight vs premultiplied** association (short YAVG sample: if `premultiply` removes ≥15% mean luma → straight) and flattened onto black accordingly. Straight gets `premultiply`, already-premul only drops A (no double-darken). Native VP9 without libvpx drops α and compares transparent-region YUV trash. GigaMux imports sidecars into the preview cache on list refresh / farm poll. Redeploy `GigaMux.js` and update worker `gigamux-check` after this change.

**Hybrid α / OBS association:** OBS Spout/α WebM captures (Custom FFmpeg → `webm` + `libvpx-vp9`, e.g. `n-frames=0 deadline=realtime tile-columns=2 tile-rows=1 auto-alt-ref=0 error-resilient=1`, `libopus`) often store **straight** RGB under low alpha, so the file can look brighter than the live Spout source; QSV+libvpx hybrid delivers are often already premul-like and closer to Spout. Per-side association flatten makes VMAF compare the same black composite instead of punishing that mismatch. α still needs a BGRA / full-range canvas (`yuva420p`); `auto-alt-ref=0` is the usual alpha-safe flag.

`mux` / `remux` run `setts=TS-STARTPTS` so A/V start times land at zero; residual A/V start / container skew on Check is a Low WARN only (does not hard-fail or block cleanup). Farm `final` compares **video-stream** durations (not audio-led container tags). Local MP4 encode and farm MP4 remux use `-movflags +use_metadata_tags` so `GigaMux_version` / `GigaMux_*` tags show in ffprobe (Lavf may still overwrite the standard `encoder` tag).

Flamenco (not GigaMux) soft-fails a task across ~3 workers before marking the task `failed`. The **job** only goes `failed` when enough tasks have failed (Manager percentage threshold) or no worker can finish it; one failed `final-check` among dozens of chunk tasks can leave the job `active`.

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
| Chunk QSV color (multi-QSV) | off | Hybrid only: also split QSV color IVF when Chunk encode is on |

Skip if the final deliver already exists (submitter). Chunk argv is submitted as `-n`; `ensure-chunk` / `ensure-out` only force `-y` when rebuilding a bad/missing file. Batch submit shows a progress bar (and taskbar fill) and keeps the UI responsive; Esc cancels remaining jobs after the in-flight POST.

## Cancel

Workers poll ~10s, then kill the `exec` child. A few seconds of continued encode after Cancel is normal.

Upstream Flamenco currently has a bug where the Windows workers are not killed upon task/job cancel (Linux was usually fine). Windows workers need to be fixed upstream for this. Thus, GigaMux warns when working with Flamenco while the Manager is not in beta. This will be the remaining behavior until it's either fixed upstream, or until I wish to integreate a workaround that doesn't depend on an upstream fix, and/or that will work with Flamenco 3.9 and below.

## Limitations

- Local (non-Flamenco) encode has no chunking.
- Hybrid farm jobs need workers with both `qsv` and `cpu` (and `gigamux-check` that supports `mux-alpha-webm`).
- No Manager write API for worker `task_types` yet; Probe Farm only recommends/exports YAML ([#104469](https://projects.blender.org/studio/flamenco/pulls/104469)). Automagic apply can return once Manager can reach each worker’s API.
- Workers must not share one [cwd](https://en.wikipedia.org/wiki/Working_directory) / `flamenco-worker.yaml` (shared binary OK).
- Farm probe uses one job per worker via temporary `gm-…` pin tags; pins are deleted after each job. Batch end merges JSON and opens YAML recommendations (hostname ↔ Manager worker name).
- Encode task types are derived from Targets NVENC generation (not a global Send combo). Test mode submits one farm job per slot.
- No seam-scan on the farm (Python-only locally today).
- Local VMAF (manual or idle auto) scores leftover `*.cNNNofMMM.*` chunks against the source frame range implied by the filename; Check/`check_chunk_file` must pass first (incomplete mid-farm chunks are skipped / blocked like bad finals).
- Attached farm VMAF requires `libvmaf` on workers that accept task_type `vmaf`; Separate/batch use `vmaf` only.
- SSDP Find does not configure Manager; URL is written to prefs when a Manager answers `/api/v3/version`.
- Whether or not cancellation works depends on Worker build and (for ≤3.9) GigaMux’s legacy circumvention path.
