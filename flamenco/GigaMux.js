// SPDX-License-Identifier: Unlicense
// Job type GigaMux: one misc/exec ffmpeg task; argv from the GigaMux submitter.
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
            key: "ffmpeg_argv_json",
            type: "string",
            required: true,
            description: "JSON array: full ffmpeg argv including leading program name",
        },
    ],
};

function compileJob(job) {
    const settings = job.settings;
    const exe = settings.ffmpeg_exe || "ffmpeg";
    const raw = settings.ffmpeg_argv_json;
    if (typeof raw !== "string" || raw.length === 0) {
        throw "GigaMux: ffmpeg_argv_json is required";
    }
    const argv = JSON.parse(raw);
    if (!Array.isArray(argv) || argv.length === 0) {
        throw "GigaMux: ffmpeg_argv_json must be a non-empty JSON array";
    }
    const args = argv.slice(1);

    print("GigaMux compileJob: exe=", exe, " nargs=", args.length);

    const task = author.Task("encode", "misc");
    task.addCommand(author.Command("exec", {
        exe: exe,
        args: args,
    }));
    job.addTask(task);
}
