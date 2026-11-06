// SPDX-License-Identifier: Unlicense
// Job type GigaMux: ffmpeg argv(s) from the GigaMux submitter.
// Single file: one misc/exec. Chunked: N encode tasks + concat (depends on all) with mux.
// Deploy via Settings → Files → Deploy GigaMux.js… (or copy into Manager scripts/).

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
            description: "JSON argv for ffconcat video join (empty when not chunking)",
        },
        {
            key: "mux_argv_json",
            type: "string",
            required: false,
            default: "",
            description: "JSON argv to mux source audio onto concatenated video (empty when not chunking)",
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

function compileJob(job) {
    const settings = job.settings;
    const exe = settings.ffmpeg_exe || "ffmpeg";
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

    const concatArgv = parseArgvJson(settings.concat_argv_json, "concat_argv_json");
    const muxArgv = parseArgvJson(settings.mux_argv_json, "mux_argv_json");
    const chunked = concatArgv !== null && muxArgv !== null;

    print(
        "GigaMux compileJob: exe=",
        exe,
        " encodes=",
        encodeArgvs.length,
        " chunked=",
        chunked,
        " cancel_exec=",
        cancelMode
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
        addFfmpegExec(task, exe, argv);
        job.addTask(task);
        encodeTasks.push(task);
    }

    if (chunked) {
        const concatTask = author.Task("concat", "misc");
        addFfmpegExec(concatTask, exe, concatArgv);
        addFfmpegExec(concatTask, exe, muxArgv);
        for (const t of encodeTasks) {
            concatTask.addDependency(t);
        }
        job.addTask(concatTask);
    }
}
