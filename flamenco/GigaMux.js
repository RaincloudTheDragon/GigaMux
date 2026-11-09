// SPDX-License-Identifier: Unlicense
// Job type GigaMux: ffmpeg argv(s) from the GigaMux submitter.
// Single file: one misc/exec (+ optional final-check).
// Chunked: N ensure-chunk encodes → mux (ensure-out vmux from ffconcat+audio) → remux (ensure-out final) → final-check/cleanup.
// mux and remux are separate Flamenco tasks so remux soft-fail never re-runs the stage join (same reason we used to split concat from mux).
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
    ],
};

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

function compileJob(job) {
    const settings = job.settings;
    const exe = settings.ffmpeg_exe || "ffmpeg";
    const checkExe =
        typeof settings.check_exe === "string" && settings.check_exe.trim().length > 0
            ? settings.check_exe.trim()
            : "gigamux-check";
    const cancelMode = cancelExecMode(settings);

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
            throw "GigaMux: encode_argvs_json or ffmpeg_argv_json is required";
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
        cleanupRaw.length > 0
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
        const task = author.Task(name, "misc");
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
        const muxTask = author.Task("mux", "misc");
        addEnsureOut(muxTask, checkExe, exe, vmuxPath, expectDur, muxArgv);
        for (const t of encodeTasks) {
            muxTask.addDependency(t);
        }
        job.addTask(muxTask);

        const remuxTask = author.Task("remux", "misc");
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
        const finalTask = author.Task("final-check", "misc");
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
    } else if (cleanupRaw.length > 0) {
        throw "GigaMux: cleanup_json requires final_check_json (cleanup runs after successful final check)";
    }
}
