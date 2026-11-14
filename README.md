<p align="center">
  <img src="assets/logo.png" width="112" alt="DSH Marina logo">
</p>

<h1 align="center">DSH Marina</h1>

<p align="center">
  A Windows desktop launcher for running isolated
  <a href="https://github.com/deepseek-ai/deepseek-harness">DeepSeek Harness (DSH)</a>
  workspaces side by side.
</p>

<p align="center">
  <img alt="License" src="https://img.shields.io/badge/license-MIT-blue">
  <img alt="Platform" src="https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078D4">
  <img alt="Rust" src="https://img.shields.io/badge/rust-1.88%2B-orange">
</p>

<p align="center">
  <strong>English</strong> · <a href="README.zh-CN.md">简体中文</a>
</p>

<p align="center">
  Source: <a href="https://github.com/QuaonY/dsh-marina">GitHub</a> ·
  <a href="https://cnb.cool/QuaonY/dsh-marina">CNB</a> ·
  <a href="https://github.com/QuaonY/dsh-marina-packs">Community packs</a>
</p>

---

DSH normally reads from a single user profile. Marina gives every workspace its own `DSH_HOME`,
Electron data directory, plugin set, and logs, so several DSH versions can run at the same time
without interfering with each other. It also handles the day-to-day work around that setup:
launching and stopping, installing runtimes, taking snapshots, recovering from crashes, and
sharing a ready-to-use configuration with someone else.

Marina is a separate launcher, not a DSH fork: it starts the official DSH build with isolated paths.
It is Windows-only (Windows 10 21H2 or later, DirectX 11) and builds into one self-contained
executable — no installer and no Visual C++ Redistributable required.

<p align="center">
  <img src="assets/screenshots/home.png" width="100%" alt="DSH Marina home screen: two isolated instances, one running, with live memory usage">
</p>

<p align="center"><sub>One window for every instance, version, plugin, and snapshot.</sub></p>

## Getting started

1. Build `dsh-marina.exe` from [source](#building-from-source).
2. Create an instance and pick a DSH runtime version, or let Marina download one.
3. Start the instance. Plugins, snapshots, packs, and the tray are all managed from this window.

## Features

### Instances

- Create, rename, clone, and delete workspaces, each with its own accent color.
- Cloning leaves out sessions, credentials, anonymous identity, and Electron login state by
  default. The option to copy personal data is only available while the source instance is
  stopped.
- Delete in three safe ways: export first, keep the data in `trash` for 30 days, or move it to the
  Windows Recycle Bin.
- A dashboard shows every instance, its runtime, and whether it is running.

### Process lifecycle

- Each instance runs in its own Windows Job Object.
- Stop sends `WM_CLOSE`, waits five seconds, then terminates the process tree.
- Instances that keep running after Marina restarts are adopted on the next launch.
- Readiness is read from the `dsh web: http://127.0.0.1:<port>/` line and confirmed by probing the
  port.
- DSH stdout and stderr go to `marina/stdout-*.log`; tokens are redacted and the file is capped at
  4 MB.

### Plugins

- List third-party plugins and enable or disable them through `cordis.patch.yml`, with an automatic
  backup before every change.
- One-click safe mode for a workspace that will not start.
- Uninstall plugins and reinstall dependencies through the runtime's bundled
  `dsh plugin --profile desktop` CLI.
- Marina refuses plugin writes while the instance is running, so a live workspace is never changed
  underneath DSH.

### Plugin market

- Browse a remote index and search by name, title, author, description, category, and declared
  capabilities. Every word in a query must match.
- Install a plugin into a chosen instance and see download and star counts.
- Run state is respected: installing and changing plugins is disabled while the target is running.

### Snapshots and rescue

- Manual snapshots plus an automatic snapshot before each launch.
- Mark a stable run as last-good.
- Attribute a crash to suspicious plugins, disable them, or roll back to last-good: restore the
  snapshot, reinstall plugin dependencies, then optionally restart.
- Optional automatic rollback, off by default.

### Runtimes

- Keep several DSH versions side by side and bind an instance to a specific version or to "latest
  stable".
- Download from the official feed (nightly or stable), GitHub Releases, a GitHub mirror prefix, or
  a custom feed, with automatic fallback.
- Resumable downloads, SHA-512 verification, and silent NSIS installation.
- Installers dropped into `runtimes/` are installed offline.
- Marina checks for a new version once a day and suggests cleanup when more than `max_runtimes`
  are installed.

### Marina Packs

- Export an instance to a `.marina-pack.zip` and import it again.
- Share a pack to the community catalog through GitHub Device Flow, with the `public_repo` scope:
  Marina publishes the archive as a Release in the user's `dsh-marina-shared-packs` repository,
  then opens a pull request with the metadata against
  [QuaonY/dsh-marina-packs](https://github.com/QuaonY/dsh-marina-packs). Validation runs
  automatically; a maintainer merges the pull request.
- The share flow scans the exported files and redacts credentials and absolute paths.

### Tray and desktop integration

- Closing the window hides Marina to the system tray; quitting is done from the tray menu.
- A tray menu per instance, plus a tooltip with the running count and total memory.
- Start with Windows, single-instance handling, and an optional exit confirmation that lists
  running instances.
- Full uninstall with separate options for the application and user data.

### About

- Version, MIT license, repository link, the full third-party license list, and a manual update
  check.

## Data and privacy

Everything lives under `%USERPROFILE%\.dsh-marina\` by default:

```text
%USERPROFILE%\.dsh-marina\
├─ marina.toml
├─ instances\
│  └─ <id>\
│     ├─ home\          DSH_HOME
│     ├─ electron\      Electron --user-data-dir
│     ├─ marina\        Marina's per-instance state and stdout logs
│     └─ snapshots\
├─ runtimes\<version>\
├─ download\
├─ trash\
├─ github_token.json    DPAPI-encrypted GitHub token
└─ marina.log
```

- The data directory can be changed in Settings. Marina moves the files, verifies the copy, and
  only then removes the old directory.
- Nothing is written next to the executable, so the exe can be moved or replaced freely.
- The GitHub token is encrypted with Windows DPAPI and is only decrypted for this Windows user.
  Logs and exported packs redact tokens, credentials, and local absolute paths.
- Marina has no analytics or telemetry. Network access happens only for what the feature at hand
  needs: update checks, runtime downloads, the plugin market, and community sharing.

## Building from source

### Prerequisites

- Windows 10 21H2 or later with a DirectX 11 driver
- Rust 1.88+ (edition 2024)
- Visual Studio Build Tools with the Windows SDK, for `rc.exe` (the window icon)

### Build

```powershell
cargo build --release
```

The result is `target\release\dsh-marina.exe`. Release builds have no console window.

The executable is fully self-contained: assets are compiled in, and `.cargo/config.toml`
static-links the MSVC C runtime. It does not need `VCRUNTIME140.dll` or the Visual C++
Redistributable. The remaining imports (`kernel32`, `user32`, `d3d11`, `dwrite`, `secur32`, and so
on) ship with Windows.

The icon is generated from `assets/logo.png` during the build. If `rc.exe` is not found, the build
still succeeds with a warning and the app uses a generic Windows icon.

Release binaries are not code-signed, so Windows SmartScreen may ask for confirmation the first
time the exe is run.

## Development

```powershell
cargo fmt --check
cargo test
cargo run
```

`cargo build` keeps a console window for logs; `cargo build --release` hides it.

### Manual testing

Automated tests cover the core logic. The following still need a real Windows desktop:

- Downloads from the different runtime sources
- NSIS silent installation
- `dsh plugin` CLI integration
- Tray behavior and GUI interactions
- Process lifecycle details (Job Objects and window messages)
- The GitHub sharing flow (Device Flow, Release upload, pull request)

## Tech stack

| Area | Used |
| --- | --- |
| UI | [gpui-kit](https://github.com/longbridge/gpui-kit) |
| System tray | [gpui-tray](https://github.com/domenkozar/gpui-tray) |
| Windows integration | `windows` crate (Job Objects, registry, shell, process APIs) |
| Networking | `reqwest` with native TLS and the Windows system proxy |
| Data formats | serde, TOML, YAML, JSON, and ZIP |

## Project layout

```text
src/            application modules (instances, process, plugins, packs, …)
src/ui/         GPUI views, dialogs, and tab pages
src/bin/        fake-desktop test double used by the integration tests
tests/          process lifecycle integration tests
assets/         logo, icons, screenshots, and the third-party license list
```

## Contributing

Issues and pull requests are welcome.

## License

MIT. See [LICENSE](LICENSE).

## Acknowledgments

- Author and maintainer: Quaon
- DeepSeek Harness is developed by DeepSeek: [deepseek-ai/deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)
- Built on GPUI
- Uses the gpui-kit component library
- Third-party licenses: [assets/THIRD-PARTY-LICENSES.txt](assets/THIRD-PARTY-LICENSES.txt)
