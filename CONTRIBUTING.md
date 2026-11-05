# Contributing to GigaMux

Thanks for helping. Before you start:

1. GigaMux is released under the [Unlicense](LICENSE) (public domain). By contributing, you dedicate your changes the same way, or confirm they are already public-domain / Unlicense-compatible. There is no CLA.
2. Read the [README](README.md). If you touch encode defaults, Maxrate, CQ, or Test mode, also read [Comparing NVENC AV1 and HEVC](docs/codec-efficiency.md).
3. For non-trivial features, open an issue or sketch the design intent before a large PR. Small bugfixes and docs can go straight to a pull request.

## Setup

The desktop shell is Windows-focused. Linux support will come when I've got the time to hop into my Arch install. Darwin will not be officially supported by me, but I won't reject Darwin-related PRs.

| | |
| --- | --- |
| Rust | stable via [rustup](https://rustup.rs) (edition 2021) |
| ffmpeg | on `PATH` |
| VMAF | full ffmpeg with `libvmaf` (e.g. [gyan.dev full](https://www.gyan.dev/ffmpeg/builds/)) |

```sh
cargo run -p GigaMux                 # desktop app
cargo run --release -p GigaMux       # when timing encodes
cargo test --workspace
```

Workspace members: `apps/GigaMux` (egui shell), `crates/gigamux-core` (encode / Check / VMAF / prefs).

## Quality expectations

Before asking for review, locally:

- `cargo fmt`
- `cargo clippy --workspace --all-targets` — fix warnings you introduce (CI does not yet enforce `-D warnings`)
- `cargo test --workspace`
- Smoke the UI path you changed (Encode, Check, VMAF, and/or Test mode as relevant)

## Code norms

- Match surrounding style. Comments explain *why*, not what the code already says.
- Prefer `Result` and logged / surfaced errors for ffmpeg, filesystem paths, and other external or user input. Do not add panics on hostile input.
- Existing `unwrap` / `expect` on internal UI invariants is fine to leave; do not spread that pattern to fallible I/O or parse paths.
- Do not commit media fixtures, secrets, credentials, or large binaries.
- Call out in the PR if you change skip-if-exists, Test mode `test1_` / `test2_` naming, or Check/VMAF pairing behavior.

## Pull requests

- One topic per PR. Split unrelated fixes.
- Keep pure refactors separate from behavior changes when practical.
- Description should cover the problem, the approach, and any limits or rejected alternatives when those are not obvious.
- Attach before/after screenshots only when the UI visibly changes.

## Commits

- Small, focused commits with a clear subject. Area prefixes are fine: `encode:`, `ui:`, `docs:`, `check:`, `vmaf:`.
- Leave the tree green (fmt / clippy / tests / smoke) before requesting review.
