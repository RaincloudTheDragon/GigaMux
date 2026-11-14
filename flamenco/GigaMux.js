// SPDX-License-Identifier: Unlicense
// Job type GigaMux: ffmpeg argv(s) from the GigaMux submitter.
// Single file: encode task_type from settings → optional final-check (cpu) → optional trailing vmaf.
// Chunked: N encode-c* → mux/remux/final-check (cpu) → optional vmaf.
// Hybrid: hybrid_json → encode-color (qsv) + encode-alpha (cpu) → remux-alpha → audio → final.
// VMAF-only: vmaf_pairs_json → ensure-vmaf (or vmaf-p* + vmaf-merge when parts[]); jobs are tagless.
// Temporal parts: same frame range on ref+dist; merge writes final .vmaf.json and deletes part sidecars.
// Farm-probe stays misc + gm-… pin tag (Probe Farm isolation only).
// mux and remux are separate Flamenco tasks so remux soft-fail never re-runs the stage join.
// Deploy via Settings → Files → Deploy GigaMux… (Manager install → scripts/ + tools/ helpers).

const JOB_TYPE = {
    label: "GigaMux",
    description: "Encode one media file with ffmpeg argv authored by GigaMux (stock Flamenco worker)",
    settings: [
        {
            key: "ffmpeg_exe",
            type: "string",
            required: true,
            default: "ffmpeg",
            description: "ffmpeg binary on the worker (PATH name or absolute path)",
        },
        {
            key: "encode_argvs_json",
            type: "string",
            required: false,
            default: "",
            description: "JSON array of argv arrays (each includes leading program name). Length 1 = single encode.",
        },
        {
            key: "concat_argv_json",
            type: "string",
            required: false,
            default: "",
            description: "Deprecated/empty — stage mux reads ffconcat directly (no .vcat)",
        },
        {
            key: "mux_argv_json",
            type: "string",
            required: false,
            default: "",
            description: "JSON argv: stage mux ffconcat+audio/subs → {stem}.vmux.mkv",
        },
        {
            key: "mux_remux_argv_json",
            type: "string",
            required: false,
            default: "",
            description: "JSON argv: remux .vmux.mkv → final deliver (-c copy); separate task",
        },
        {
            key: "out_expect_dur",
            type: "float",
            required: false,
            default: 0,
            description: "Expected duration (seconds) for ensure-out on vmux / final",
        },
        {
            key: "ffmpeg_argv_json",
            type: "string",
            required: false,
            default: "",
            description: "Legacy single argv JSON (used when encode_argvs_json is empty)",
        },
        {
            key: "cancel_exec",
            type: "string",
            required: false,
            default: "direct",
            description: "direct = ffmpeg exec (Worker kill PR / 3.10-beta); legacy_stub = ≤3.9 placeholder until cancel helper exists",
        },
        {
            key: "check_exe",
            type: "string",
            required: false,
            default: "gigamux-check",
            description: "Worker binary for chunk/final integrity + cleanup (PATH or Flamenco tools/)",
        },
        {
            key: "chunk_checks_json",
            type: "string",
            required: false,
            default: "",
            description: "JSON array of {path, expect_dur} parallel to encode tasks (same-task trailing check)",
        },
        {
            key: "final_check_json",
            type: "string",
            required: false,
            default: "",
            description: "JSON FarmFinalCheck for input→output integrity (separate task; job fails on error)",
        },
        {
            key: "cleanup_json",
            type: "string",
            required: false,
            default: "",
            description: "JSON FarmCleanup: runs after successful final check only",
        },
        {
            key: "vmaf_json",
            type: "string",
            required: false,
            default: "",
            description: "JSON {input,output,sidecar,parts?}: trailing VMAF after final-check (task_type vmaf); parts → parallel ensure-vmaf-part + merge-vmaf",
        },
        {
            key: "vmaf_pairs_json",
            type: "string",
            required: false,
            default: "",
            description: "JSON array of {input,output,sidecar,parts?}; VMAF-only job (no encode); parts → parallel temporal slices",
        },
        {
            key: "encode_task_type",
            type: "string",
            required: false,
            default: "cpu",
            description: "Flamenco task type for encode / encode-c* (nvenc, nvenc-{gen}, qsv, cpu)",
        },
        {
            key: "probe_out_dir",
            type: "string",
            required: false,
            default: "",
            description: "When set: farm-probe only; gigamux-check farm-probe <dir> (no encode)",
        },
        {
            key: "hybrid_json",
            type: "string",
            required: false,
            default: "",
            description:
                "QSV+libvpx alpha hybrid graph JSON: color_tasks, alpha_tasks, optional concat argvs, remux_check_args, audio_argv, video_expect_dur, out_expect_dur",
        },
    ],
};

// Encode pool task type from submitter (fallback cpu).
function encodeTaskType(settings) {
    const raw =
        typeof settings.encode_task_type === "string" ? settings.encode_task_type.trim() : "";
    return raw.length > 0 ? raw : "cpu";
}

function parseArgvJson(raw, label) {
    if (typeof raw !== "string" || raw.trim().length === 0) {
        return null;
    }
    const argv = JSON.parse(raw);
    if (!Array.isArray(argv) || argv.length === 0) {
        throw "GigaMux: " + label + " must be a non-empty JSON array";
    }
    return argv;
}

function execArgsFromArgv(argv) {
    // argv[0] is logical program name; worker uses settings.ffmpeg_exe.
    return argv.slice(1);
}

function cancelExecMode(settings) {
    const raw = settings.cancel_exec;
    if (typeof raw === "string" && raw.trim() === "legacy_stub") {
        return "legacy_stub";
    }
    return "direct";
}

function addFfmpegExec(task, exe, argv) {
    // Direct ffmpeg exe (no shell): Flamenco CommandContext / Worker kill PR can stop it on Cancel (~10s poll).
    // legacy_stub uses the same path until a cancel helper ships (submitter warns on ≤3.9).
    task.addCommand(
        author.Command("exec", {
            exe: exe,
            args: execArgsFromArgv(argv),
        })
    );
}

function addCheckExec(task, checkExe, args) {
    task.addCommand(
        author.Command("exec", {
            exe: checkExe,
            args: args,
        })
    );
}

function parseChunkChecks(raw) {
    if (typeof raw !== "string" || raw.trim().length === 0) {
        return null;
    }
    const parsed = JSON.parse(raw);
    if (!Array.isArray(parsed)) {
        throw "GigaMux: chunk_checks_json must be a JSON array";
    }
    return parsed;
}

// Last path-like token in an ffmpeg argv (output file).
function argvOutputPath(argv) {
    if (!Array.isArray(argv) || argv.length === 0) {
        return null;
    }
    return argv[argv.length - 1];
}

function outExpectDur(settings, chunkChecks) {
    const raw = settings.out_expect_dur;
    if (typeof raw === "number" && raw > 0) {
        return raw;
    }
    if (typeof raw === "string" && raw.trim().length > 0) {
        const n = parseFloat(raw);
        if (!isNaN(n) && n > 0) {
            return n;
        }
    }
    if (chunkChecks !== null) {
        let sum = 0;
        for (let i = 0; i < chunkChecks.length; i++) {
            const d = chunkChecks[i] && chunkChecks[i].expect_dur;
            if (typeof d === "number") {
                sum += d;
            }
        }
        if (sum > 0) {
            return sum;
        }
    }
    throw "GigaMux: out_expect_dur required for chunked ensure-out (or chunk_checks_json with expect_dur)";
}

function addEnsureOut(task, checkExe, exe, outPath, expectDur, argv) {
    // Discrete args after `--` — nested JSON breaks when Flamenco expands {shared_storage_path} to `W:\…`.
    addCheckExec(task, checkExe, [
        "ensure-out",
        outPath,
        String(expectDur),
        exe,
        "--",
    ].concat(argv));
}

function normalizeVmafParts(spec, label) {
    if (!spec.parts) {
        spec.parts = [];
        return;
    }
    if (!Array.isArray(spec.parts)) {
        throw "GigaMux: " + label + " parts must be an array";
    }
    for (let i = 0; i < spec.parts.length; i++) {
        const p = spec.parts[i];
        if (!p || typeof p.sidecar !== "string" || !p.sidecar) {
            throw "GigaMux: " + label + " parts[" + i + "] needs sidecar";
        }
        if (typeof p.start_frame !== "number" || typeof p.frame_count !== "number" || typeof p.fps !== "number") {
            throw "GigaMux: " + label + " parts[" + i + "] needs start_frame, frame_count, fps numbers";
        }
    }
}

function parseVmafSpec(raw, label) {
    if (typeof raw !== "string" || raw.trim().length === 0) {
        return null;
    }
    let spec;
    try {
        spec = JSON.parse(raw);
    } catch (e) {
        throw "GigaMux: " + label + ": " + e;
    }
    if (!spec || typeof spec.input !== "string" || typeof spec.output !== "string" || typeof spec.sidecar !== "string") {
        throw "GigaMux: " + label + " needs input, output, sidecar strings";
    }
    if (!spec.input || !spec.output || !spec.sidecar) {
        throw "GigaMux: " + label + " paths must be non-empty";
    }
    normalizeVmafParts(spec, label);
    return spec;
}

function parseVmafPairs(raw) {
    if (typeof raw !== "string" || raw.trim().length === 0) {
        return null;
    }
    let arr;
    try {
        arr = JSON.parse(raw);
    } catch (e) {
        throw "GigaMux: vmaf_pairs_json: " + e;
    }
    if (!Array.isArray(arr) || arr.length === 0) {
        throw "GigaMux: vmaf_pairs_json must be a non-empty JSON array";
    }
    const out = [];
    for (let i = 0; i < arr.length; i++) {
        const p = arr[i];
        if (!p || typeof p.input !== "string" || typeof p.output !== "string" || typeof p.sidecar !== "string") {
            throw "GigaMux: vmaf_pairs_json[" + i + "] needs input, output, sidecar";
        }
        normalizeVmafParts(p, "vmaf_pairs_json[" + i + "]");
        out.push(p);
    }
    return out;
}

function addEnsureVmaf(task, checkExe, exe, spec) {
    addCheckExec(task, checkExe, [
        "ensure-vmaf",
        spec.input,
        spec.output,
        spec.sidecar,
        exe,
    ]);
}

function addEnsureVmafPart(task, checkExe, exe, spec, part) {
    addCheckExec(task, checkExe, [
        "ensure-vmaf-part",
        spec.input,
        spec.output,
        part.sidecar,
        String(part.start_frame),
        String(part.frame_count),
        String(part.fps),
        exe,
    ]);
}

function addMergeVmaf(task, checkExe, spec) {
    const args = ["merge-vmaf", spec.sidecar];
    for (let i = 0; i < spec.parts.length; i++) {
        args.push(spec.parts[i].sidecar);
    }
    addCheckExec(task, checkExe, args);
}

// Attach full-file ensure-vmaf, or parallel vmaf-p* + vmaf-merge when parts are present.
function addVmafPairGraph(job, checkExe, exe, spec, namePrefix, dependOn) {
    const parts = Array.isArray(spec.parts) ? spec.parts : [];
    if (parts.length === 0) {
        const task = author.Task(namePrefix, "vmaf");
        addEnsureVmaf(task, checkExe, exe, spec);
        if (dependOn) {
            task.addDependency(dependOn);
        }
        job.addTask(task);
        return;
    }
    const partTasks = [];
    for (let i = 0; i < parts.length; i++) {
        const pad = ("000" + i).slice(-3);
        const name = namePrefix + "-p" + pad;
        const task = author.Task(name, "vmaf");
        addEnsureVmafPart(task, checkExe, exe, spec, parts[i]);
        if (dependOn) {
            task.addDependency(dependOn);
        }
        job.addTask(task);
        partTasks.push(task);
    }
    const merge = author.Task(namePrefix + "-merge", "vmaf");
    addMergeVmaf(merge, checkExe, spec);
    for (let i = 0; i < partTasks.length; i++) {
        merge.addDependency(partTasks[i]);
    }
    job.addTask(merge);
}

function addHybridEncodeTask(job, spec, exe, checkExe) {
    if (!spec || typeof spec.name !== "string" || typeof spec.task_type !== "string") {
        throw "GigaMux: hybrid encode task needs name + task_type";
    }
    if (!Array.isArray(spec.argv) || spec.argv.length === 0) {
        throw "GigaMux: hybrid encode task " + spec.name + " needs argv";
    }
    const taskType = spec.task_type.trim().length > 0 ? spec.task_type.trim() : "cpu";
    const task = author.Task(spec.name, taskType);
    if (spec.ensure && typeof spec.ensure.path === "string" && typeof spec.ensure.expect_dur === "number") {
        addCheckExec(task, checkExe, [
            "ensure-chunk",
            spec.ensure.path,
            String(spec.ensure.expect_dur),
            exe,
            "--",
        ].concat(spec.argv));
    } else {
        addFfmpegExec(task, exe, spec.argv);
    }
    job.addTask(task);
    return task;
}

function compileHybridJob(job, settings, exe, checkExe, cancelMode, vmafAttached, hybridRaw) {
    let hybrid;
    try {
        hybrid = JSON.parse(hybridRaw);
    } catch (e) {
        throw "GigaMux: hybrid_json: " + e;
    }
    if (!hybrid || !Array.isArray(hybrid.color_tasks) || !Array.isArray(hybrid.alpha_tasks)) {
        throw "GigaMux: hybrid_json needs color_tasks + alpha_tasks arrays";
    }
    if (hybrid.color_tasks.length === 0 || hybrid.alpha_tasks.length === 0) {
        throw "GigaMux: hybrid_json color/alpha tasks must be non-empty";
    }
    if (!Array.isArray(hybrid.remux_check_args) || hybrid.remux_check_args.length < 4) {
        throw "GigaMux: hybrid_json remux_check_args needs mux-alpha-webm + 3 paths";
    }
    if (!Array.isArray(hybrid.audio_argv) || hybrid.audio_argv.length === 0) {
        throw "GigaMux: hybrid_json audio_argv required";
    }
    // IVF concat is video-only; final audio mux may track longer source audio (container duration).
    const videoExpectDur =
        typeof hybrid.video_expect_dur === "number" && hybrid.video_expect_dur > 0
            ? hybrid.video_expect_dur
            : typeof hybrid.out_expect_dur === "number" && hybrid.out_expect_dur > 0
              ? hybrid.out_expect_dur
              : outExpectDur(settings, null);
    print(
        "GigaMux compileJob: hybrid color=",
        hybrid.color_tasks.length,
        " alpha=",
        hybrid.alpha_tasks.length,
        " cancel_exec=",
        cancelMode,
        " check_exe=",
        checkExe
    );
    if (cancelMode === "legacy_stub") {
        print("GigaMux: cancel_exec=legacy_stub — Windows cancel helper not shipped; using direct ffmpeg exec");
    }

    const colorTasks = [];
    for (let i = 0; i < hybrid.color_tasks.length; i++) {
        colorTasks.push(addHybridEncodeTask(job, hybrid.color_tasks[i], exe, checkExe));
    }
    const alphaTasks = [];
    for (let i = 0; i < hybrid.alpha_tasks.length; i++) {
        alphaTasks.push(addHybridEncodeTask(job, hybrid.alpha_tasks[i], exe, checkExe));
    }

    let colorReady = colorTasks;
    if (Array.isArray(hybrid.color_concat_argv) && hybrid.color_concat_argv.length > 0) {
        const concatTask = author.Task("concat-color", "cpu");
        const outPath = argvOutputPath(hybrid.color_concat_argv);
        addEnsureOut(concatTask, checkExe, exe, outPath, videoExpectDur, hybrid.color_concat_argv);
        for (let i = 0; i < colorTasks.length; i++) {
            concatTask.addDependency(colorTasks[i]);
        }
        job.addTask(concatTask);
        colorReady = [concatTask];
    }

    let alphaReady = alphaTasks;
    if (Array.isArray(hybrid.alpha_concat_argv) && hybrid.alpha_concat_argv.length > 0) {
        const concatTask = author.Task("concat-alpha", "cpu");
        const outPath = argvOutputPath(hybrid.alpha_concat_argv);
        addEnsureOut(concatTask, checkExe, exe, outPath, videoExpectDur, hybrid.alpha_concat_argv);
        for (let i = 0; i < alphaTasks.length; i++) {
            concatTask.addDependency(alphaTasks[i]);
        }
        job.addTask(concatTask);
        alphaReady = [concatTask];
    }

    const remuxTask = author.Task("remux-alpha", "cpu");
    addCheckExec(remuxTask, checkExe, hybrid.remux_check_args);
    for (let i = 0; i < colorReady.length; i++) {
        remuxTask.addDependency(colorReady[i]);
    }
    for (let i = 0; i < alphaReady.length; i++) {
        remuxTask.addDependency(alphaReady[i]);
    }
    job.addTask(remuxTask);

    const audioOut = argvOutputPath(hybrid.audio_argv);
    const audioTask = author.Task("audio", "cpu");
    // Ensure against video length — container may still track longer source audio; final-check owns A/V skew.
    addEnsureOut(audioTask, checkExe, exe, audioOut, videoExpectDur, hybrid.audio_argv);
    audioTask.addDependency(remuxTask);
    job.addTask(audioTask);

    let gateTask = audioTask;
    const finalCheckRaw =
        typeof settings.final_check_json === "string" ? settings.final_check_json.trim() : "";
    const cleanupRaw =
        typeof settings.cleanup_json === "string" ? settings.cleanup_json.trim() : "";

    if (finalCheckRaw.length > 0) {
        let finalSpec;
        try {
            finalSpec = JSON.parse(finalCheckRaw);
        } catch (e) {
            throw "GigaMux: final_check_json: " + e;
        }
        const finalTask = author.Task("final-check", "cpu");
        addCheckExec(finalTask, checkExe, [
            "final",
            finalSpec.input,
            finalSpec.output,
            finalSpec.opaque_video,
            finalSpec.opaque_container,
            finalSpec.alpha_video,
            finalSpec.alpha_container,
        ]);
        if (cleanupRaw.length > 0) {
            let cleanSpec;
            try {
                cleanSpec = JSON.parse(cleanupRaw);
            } catch (e) {
                throw "GigaMux: cleanup_json: " + e;
            }
            addCheckExec(finalTask, checkExe, [
                "cleanup",
                cleanSpec.dir,
                cleanSpec.stem,
                cleanSpec.ext,
                String(cleanSpec.chunks),
            ]);
        }
        finalTask.addDependency(gateTask);
        job.addTask(finalTask);
        gateTask = finalTask;
    } else if (cleanupRaw.length > 0) {
        throw "GigaMux: cleanup_json requires final_check_json (cleanup runs after successful final check)";
    }

    if (vmafAttached !== null) {
        addVmafPairGraph(job, checkExe, exe, vmafAttached, "vmaf", gateTask);
    }
}

function compileJob(job) {
    const settings = job.settings;
    const exe = settings.ffmpeg_exe || "ffmpeg";
    const checkExe =
        typeof settings.check_exe === "string" && settings.check_exe.trim().length > 0
            ? settings.check_exe.trim()
            : "gigamux-check";
    const cancelMode = cancelExecMode(settings);
    const vmafPairs = parseVmafPairs(settings.vmaf_pairs_json);
    const vmafAttached = parseVmafSpec(settings.vmaf_json, "vmaf_json");

    // Farm HW probe: write {dir}/{host}.json for GigaMux inventory (no encode).
    const probeOut =
        typeof settings.probe_out_dir === "string" ? settings.probe_out_dir.trim() : "";
    if (probeOut.length > 0) {
        print("GigaMux compileJob: farm-probe out=", probeOut, " check_exe=", checkExe);
        const task = author.Task("farm-probe", "misc");
        addCheckExec(task, checkExe, ["farm-probe", probeOut]);
        job.addTask(task);
        return;
    }

    // VMAF-only job: no encode graph (task_type vmaf; job is tagless).
    if (vmafPairs !== null) {
        print("GigaMux compileJob: vmaf-only pairs=", vmafPairs.length, " check_exe=", checkExe);
        for (let i = 0; i < vmafPairs.length; i++) {
            const name = vmafPairs.length === 1 ? "vmaf" : "vmaf-" + i;
            const nParts = Array.isArray(vmafPairs[i].parts) ? vmafPairs[i].parts.length : 0;
            if (nParts > 0) {
                print("GigaMux compileJob: pair", i, "temporal parts=", nParts);
            }
            addVmafPairGraph(job, checkExe, exe, vmafPairs[i], name, null);
        }
        return;
    }

    // QSV color + libvpx alpha hybrid (per-task types; Rust remux via gigamux-check).
    const hybridRaw =
        typeof settings.hybrid_json === "string" ? settings.hybrid_json.trim() : "";
    if (hybridRaw.length > 0) {
        compileHybridJob(job, settings, exe, checkExe, cancelMode, vmafAttached, hybridRaw);
        return;
    }

    const encType = encodeTaskType(settings);

    let encodeArgvs = null;
    const encRaw = settings.encode_argvs_json;
    if (typeof encRaw === "string" && encRaw.trim().length > 0) {
        const parsed = JSON.parse(encRaw);
        if (!Array.isArray(parsed) || parsed.length === 0) {
            throw "GigaMux: encode_argvs_json must be a non-empty JSON array of argv arrays";
        }
        encodeArgvs = parsed;
    } else {
        // Legacy single-task payload.
        const legacy = parseArgvJson(settings.ffmpeg_argv_json, "ffmpeg_argv_json");
        if (!legacy) {
            throw "GigaMux: encode_argvs_json or ffmpeg_argv_json is required (or vmaf_pairs_json for VMAF-only)";
        }
        encodeArgvs = [legacy];
    }

    const muxArgv = parseArgvJson(settings.mux_argv_json, "mux_argv_json");
    const muxRemuxArgv = parseArgvJson(settings.mux_remux_argv_json, "mux_remux_argv_json");
    // Chunked when stage mux + remux are both present (concat_argv_json is unused / empty).
    const chunked = muxArgv !== null && muxRemuxArgv !== null;
    if (muxArgv !== null && muxRemuxArgv === null) {
        throw "GigaMux: mux_remux_argv_json required when mux_argv_json is set (MKV stage → final remux)";
    }
    const chunkChecks = parseChunkChecks(settings.chunk_checks_json);
    if (chunkChecks !== null && chunkChecks.length !== encodeArgvs.length) {
        throw (
            "GigaMux: chunk_checks_json length " +
            chunkChecks.length +
            " != encode count " +
            encodeArgvs.length
        );
    }
    const finalCheckRaw =
        typeof settings.final_check_json === "string" ? settings.final_check_json.trim() : "";
    const cleanupRaw =
        typeof settings.cleanup_json === "string" ? settings.cleanup_json.trim() : "";

    print(
        "GigaMux compileJob: exe=",
        exe,
        " encodes=",
        encodeArgvs.length,
        " chunked=",
        chunked,
        " cancel_exec=",
        cancelMode,
        " chunk_checks=",
        chunkChecks ? chunkChecks.length : 0,
        " final_check=",
        finalCheckRaw.length > 0,
        " cleanup=",
        cleanupRaw.length > 0,
        " vmaf_attached=",
        vmafAttached !== null,
        " encode_task_type=",
        encType
    );
    if (cancelMode === "legacy_stub") {
        print("GigaMux: cancel_exec=legacy_stub — Windows cancel helper not shipped; using direct ffmpeg exec");
    }

    const encodeTasks = [];
    for (let i = 0; i < encodeArgvs.length; i++) {
        const argv = encodeArgvs[i];
        if (!Array.isArray(argv) || argv.length === 0) {
            throw "GigaMux: encode argv " + i + " is empty";
        }
        const name = encodeArgvs.length === 1 ? "encode" : "encode-c" + i;
        const task = author.Task(name, encType);
        // Chunked: ensure-chunk reuses a good existing file; only forces `-y` encode when missing/bad.
        // Soft-fail requeues this single command (hybrid auto-retry).
        if (chunkChecks !== null) {
            const c = chunkChecks[i];
            if (!c || typeof c.path !== "string" || typeof c.expect_dur !== "number") {
                throw "GigaMux: chunk_checks_json[" + i + "] needs path + expect_dur";
            }
            addCheckExec(task, checkExe, [
                "ensure-chunk",
                c.path,
                String(c.expect_dur),
                exe,
                "--",
            ].concat(argv));
        } else {
            addFfmpegExec(task, exe, argv);
        }
        job.addTask(task);
        encodeTasks.push(task);
    }

    let gateTask = null;
    if (chunked) {
        const expectDur = outExpectDur(settings, chunkChecks);
        const vmuxPath = argvOutputPath(muxArgv);
        const finalPath = argvOutputPath(muxRemuxArgv);
        if (!vmuxPath || !finalPath) {
            throw "GigaMux: mux/remux argv missing output path";
        }

        // Separate mux vs remux so remux soft-fail does not re-run the stage join (ffconcat+audio → vmux).
        const muxTask = author.Task("mux", "cpu");
        addEnsureOut(muxTask, checkExe, exe, vmuxPath, expectDur, muxArgv);
        for (const t of encodeTasks) {
            muxTask.addDependency(t);
        }
        job.addTask(muxTask);

        const remuxTask = author.Task("remux", "cpu");
        addEnsureOut(remuxTask, checkExe, exe, finalPath, expectDur, muxRemuxArgv);
        remuxTask.addDependency(muxTask);
        job.addTask(remuxTask);
        gateTask = remuxTask;
    } else if (encodeTasks.length === 1) {
        gateTask = encodeTasks[0];
    }

    // Separate final check: fails the job (Manager may soft-retry this task only). Cleanup only if check passed.
    // Discrete path args (not JSON) so two-way expansion cannot break escapes.
    if (finalCheckRaw.length > 0) {
        if (!gateTask) {
            throw "GigaMux: final_check_json requires encode/mux tasks";
        }
        let finalSpec;
        try {
            finalSpec = JSON.parse(finalCheckRaw);
        } catch (e) {
            throw "GigaMux: final_check_json: " + e;
        }
        const finalTask = author.Task("final-check", "cpu");
        addCheckExec(finalTask, checkExe, [
            "final",
            finalSpec.input,
            finalSpec.output,
            finalSpec.opaque_video,
            finalSpec.opaque_container,
            finalSpec.alpha_video,
            finalSpec.alpha_container,
        ]);
        if (cleanupRaw.length > 0) {
            let cleanSpec;
            try {
                cleanSpec = JSON.parse(cleanupRaw);
            } catch (e) {
                throw "GigaMux: cleanup_json: " + e;
            }
            addCheckExec(finalTask, checkExe, [
                "cleanup",
                cleanSpec.dir,
                cleanSpec.stem,
                cleanSpec.ext,
                String(cleanSpec.chunks),
            ]);
        }
        finalTask.addDependency(gateTask);
        job.addTask(finalTask);
        gateTask = finalTask;
    } else if (cleanupRaw.length > 0) {
        throw "GigaMux: cleanup_json requires final_check_json (cleanup runs after successful final check)";
    }

    // Trailing VMAF after final-check (or encode/remux when no final-check); task_type vmaf.
    if (vmafAttached !== null) {
        if (!gateTask) {
            throw "GigaMux: vmaf_json requires encode/mux/final-check tasks";
        }
        addVmafPairGraph(job, checkExe, exe, vmafAttached, "vmaf", gateTask);
    }
}
