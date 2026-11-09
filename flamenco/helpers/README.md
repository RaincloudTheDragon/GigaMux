# Flamenco worker helpers

**Deploy GigaMux…** copies these into the Manager install `tools/` folder (alongside `scripts/GigaMux.js`).

| File | Platform |
| --- | --- |
| `gigamux-check.exe` | Windows workers |
| `gigamux-check` | Linux workers (ELF, no extension) |

## End users (release)

App releases ship both binaries next to `GigaMux.exe`:

```
GigaMux.exe
helpers/
  gigamux-check.exe
  gigamux-check
```

Use **Settings → Files → Deploy GigaMux…** and pick the Manager install folder (`flamenco-manager.yaml`). No `cargo build` required.

Deploy defaults **Worker check helper** to `{ManagerInstall}/tools/gigamux-check` (two-way rewritten). That assumes the Manager install is on shared storage. Override in Settings → Files if helpers live on another shared path. Windows uses `gigamux-check.exe`; Linux uses the extensionless ELF — both are written; the job path has no `.exe`.

## Maintainers / from-source

Build into this folder (gitignored), or rely on `target/release` / release `helpers/` search roots:

Windows (from repo root):

```bat
cargo build -p gigamux-check --release
copy target\release\gigamux-check.exe flamenco\helpers\gigamux-check.exe
```

Linux (on a Linux machine / worker):

```sh
cargo build -p gigamux-check --release
cp target/release/gigamux-check flamenco/helpers/gigamux-check
```

CI (`.github/workflows/release.yml`) builds Win + Linux helpers into the release zip under `helpers/`.

### Linux `+x` on SMB

Samba `create mask` / `force create mode` of `06xx` strips execute on create (AND-masks away the DOS Archive→owner `+x` mapping). `acl allow execute always = yes` only helps Windows clients run over SMB; it does not set the POSIX bit Linux `execve` needs on the share filesystem. Prefer something like `create mask = 0775` so Windows deploys keep owner `+x`. If a share still uses a `06xx` create mask, fix that or:

```sh
chmod a+x /path/to/Manager/tools/gigamux-check
```
