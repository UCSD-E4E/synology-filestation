# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

This is a cross-platform filesystem driver that mounts a Synology FileStation NAS share as a local directory. The core driver is written in **Rust**; an optional desktop GUI uses **.NET 10 / Avalonia**. Platform backends: Linux (FUSE via `fuser`), macOS (WebDAV via `dav-server`), Windows (WinFsp via `winfsp` crate). A **Python binding** (PyO3, see `python/synology_filestation/`) exposes the FileStation HTTP client surface as a `pip install`-able package — drop-in replacement for the `synology-api` PyPI package's FileStation operations, with typed exceptions, atomic downloads, and transparent SID-expiry recovery. The GUI talks to the Rust core **in-process** through a C ABI (`rust/synology-filestation-ffi`, a cdylib) via P/Invoke — it does not spawn the CLI.

## Development Practice

**Test-driven development is required for all behavioral changes.** Follow the red-green-refactor cycle:

1. **Red** — write a failing test that pins down the new behavior (or reproduces the bug) *before* touching implementation code. Run it and confirm it fails for the expected reason.
2. **Green** — write the minimum implementation needed to make the test pass.
3. **Refactor** — clean up while keeping the test green.

Every bug fix starts with a regression test that fails against the current code. Every new feature or API is specified by tests first. Do not write implementation code for which no failing test exists. Exceptions (pure refactors with no behavior change, docs, formatting, build/CI config) don't need new tests but must keep the existing suite green — run the relevant `cargo test` / `uv run pytest` / `dotnet test` before considering the change done.

## Repository layout

The repo is a **Cargo workspace** with four crates plus the existing .NET projects:

```
rust/synology-filestation-core/    # Pure HTTP client (no FS code, no platform deps)
rust/synology-filestation-fuse/    # CLI binary + a library exposing the FUSE/WebDAV/WinFsp mount backends
rust/synology-filestation-ffi/     # C ABI cdylib — connect/browse/transfer/mount, consumed by the GUI
rust/synology-filestation-smb/     # In-process SMB3 transport (pure Rust), incl. the framing for any byte stream
rust/synology-filestation-connect/ # Which leg reaches the NAS — SMB, SMB through a tunnel, or HTTP — and the tunnel
rust/synology-filestation-openvpn/ # In-process OpenVPN client + userspace TCP stack: SMB off campus, unprivileged
python/synology_filestation/       # PyO3 bindings (cdylib) — Python wheel builds
SynologyFuse.Gui/                  # Avalonia desktop GUI (P/Invokes the FFI cdylib)
SynologyFuse.{Mac,Deb}Installer/   # macOS .pkg / Debian .deb builders
SynologyFuse.Installer/            # Windows MSI/bundle builder (WiX 4)
```

## Build Commands

### Rust CLI

```bash
cargo build --release -p synology-filestation-fuse
cargo build --release -p synology-filestation-ffi   # cdylib the GUI loads (libsynology_filestation_ffi.{so,dylib,dll})
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

On Windows, build from a **Developer Command Prompt for VS** — Git for Windows ships a `link.exe` that shadows MSVC's linker. See `rust/synology-filestation-fuse/.cargo/config.toml` for context and the fix if linker issues arise.

### Python bindings (uv) — run from `python/synology_filestation/`

```bash
uv sync --group dev      # install deps + build wheel
uv run pytest -v         # run the binding test suite
maturin build --release  # produce a Linux wheel under target/wheels/
```

`uv sync` invokes maturin under the hood, so a fresh `import synology_filestation` works after a sync. The Rust core's tests cover the auto-relogin / atomic-download contract; the pytest suite verifies the Python-facing exception hierarchy and ergonomics.

### .NET GUI

```bash
dotnet publish SynologyFuse.Gui -c Release -r win-x64 -p:SelfContained=true
dotnet publish SynologyFuse.Gui -c Release -r linux-x64 -p:SelfContained=true
dotnet publish SynologyFuse.Gui -c Release -r osx-arm64 -p:SelfContained=true
dotnet test SynologyFuse.Tests --verbosity normal
dotnet format SynologyFuse.Gui
```

### macOS Installer (pkgbuild / productbuild)

```bash
# One-time: Xcode Command Line Tools (provides pkgbuild, productbuild, iconutil)
xcode-select --install

# Build .pkg (--build also compiles Rust CLI and publishes .NET GUI)
./SynologyFuse.MacInstaller/Build-Installer.sh --build

# Explicit version or Intel target
./SynologyFuse.MacInstaller/Build-Installer.sh --build --version 1.0.0 --arch x86_64
```

Key files in `SynologyFuse.MacInstaller/`:
- `Build-Installer.sh` — assembles `.app` bundle, runs `pkgbuild` + `productbuild`
- `Info.plist` — `.app` bundle template (`__VERSION__` / `__GUI_BINARY__` substituted at build time)
- `distribution.xml` — installer UI config (welcome screen, license, macOS 12+ requirement)
- `scripts/postinstall` — symlinks CLI to `/usr/local/bin/` after payload is placed

### Debian/Ubuntu Package (.deb)

```bash
# One-time: dpkg-dev (provides dpkg-deb)
sudo apt-get install dpkg-dev

# Build .deb (--build also compiles Rust CLI and publishes .NET GUI)
./SynologyFuse.DebInstaller/Build-Package.sh --build

# Explicit version or arm64 target
./SynologyFuse.DebInstaller/Build-Package.sh --build --version 1.0.0 --arch arm64
```

Key files in `SynologyFuse.DebInstaller/`:
- `Build-Package.sh` — stages the payload tree, writes `DEBIAN/control`, runs `dpkg-deb`
- `scripts/postinst` — refreshes icon and desktop-entry caches after install
- `scripts/prerm` — placeholder pre-removal hook

The package installs:
- CLI binary → `/usr/bin/synology-filestation-fuse`
- GUI and .NET runtime → `/opt/SynologyFuse/`
- Desktop launcher → `/usr/share/applications/synologyfuse.desktop`
- Icon → `/usr/share/icons/hicolor/256x256/apps/synologyfuse.png`

Runtime dependency: `libfuse3-3`

### Windows Installer (WiX 4)

```powershell
# One-time tool setup
dotnet tool install --global wix --version 6.0.2

# Build MSI
.\SynologyFuse.Installer\Build-Installer.ps1 -Version 1.0.0

# Build full bundle (MSI + WinFsp bootstrapper)
.\SynologyFuse.Installer\Build-Bundle.ps1
```

## Architecture

### Data Flow

```
CLI (main.rs) or GUI (MountService/SynoClient → FFI cdylib) or Python (Client/AsyncClient)
    → Tokio runtime
    → SynologyClient (rust/synology-filestation-core/src/client.rs) — async HTTP
    → Platform backend (spawned by synology-filestation-fuse's lib.rs::spawn_mount) OR PyO3 binding:
        Linux:   rust/synology-filestation-fuse/src/fs.rs       (fuser FUSE callbacks)
        macOS:   rust/synology-filestation-fuse/src/webdav.rs   (local HTTP, mounted via Finder)
        Windows: rust/synology-filestation-fuse/src/winfs.rs    (WinFsp callbacks)
        Python:  python/synology_filestation/src/lib.rs         (PyO3 _native module)
    → OS filesystem / user Python code

The GUI path goes: MountService.cs / SynoClient.cs → P/Invoke (Interop/NativeMethods.cs)
    → rust/synology-filestation-ffi/src/lib.rs (C ABI) → SynologyClient + spawn_mount.
```

### Rust core (`rust/synology-filestation-core/src/`)

| File | Role |
|------|------|
| `lib.rs`    | Module declarations + re-exports of the public surface |
| `client.rs` | Async HTTP client — all FileStation API calls, session/SID management, 2FA, auto-relogin, atomic `download_to_path`, and the opt-in transfer throttle (`ThrottleConfig`/`with_throttle`) |
| `types.rs`  | Serde structs for API responses (`SynoFileInfo`, `SynoResponse<T>`, etc.) |
| `error.rs`  | `SynoFsError` enum + Linux errno mapping (used by FUSE backend) |

### FUSE binary (`rust/synology-filestation-fuse/src/`)

| File | Role |
|------|------|
| `main.rs`   | CLI binary: parsing (clap), interactive prompts, login, then calls `lib.rs::spawn_mount` and parks on Ctrl-C |
| `lib.rs`    | Library surface: `spawn_mount`/`MountHandle` (non-blocking, background mount) + `is_otp_required`, shared by the CLI and the FFI crate |
| `fs.rs`     | Linux FUSE backend. Metadata callbacks use `runtime.block_on()`; **file transfers do not** — `flush`/`release` (upload), `setattr` (truncate) and cross-directory `rename` hand the transfer to the Tokio runtime via `start_*` and reply to the kernel from there, so no transfer ever occupies an event-loop thread. Transfers are capped at `MAX_CONCURRENT_TRANSFERS` (the event loop used to be that limit by accident) and each open handle has its own buffer lock. The session also runs a multi-threaded event loop (`MountOptions::io_threads`, `--fuse-threads`) for the remaining blocking callbacks |
| `cache.rs`  | Linux only — `InodeCache` (TTL metadata), `ReadCache` (LRU block cache, 256 KiB blocks) |
| `prefetch.rs` | Linux only — what to speculate on and when: the container sniff that decides the open window, the sequential-detection ramp behind `read`, and `InflightGuard` |
| `logfile.rs` | `--log-file`: a second `tracing` sink that outlives the process. Each record is one `write_all` + flush (no background writer thread, so a `SIGKILL`/OOM death loses nothing), warn and error are `fsync`ed (page cache does not survive a power cycle, and a mount that wedges the machine gets one), and the file rotates at 8 MiB × 3. A path inside the mountpoint is refused — logging through the mount you serve deadlocks it |
| `webdav.rs` | macOS WebDAV backend; directory moves are download→upload→delete |
| `winfs.rs`  | Windows WinFsp backend; in-memory write buffers flushed atomically on close |

### Python bindings (`python/synology_filestation/`)

| File | Role |
|------|------|
| `src/lib.rs` | PyO3 `_native` module: `Client` (sync, blocks on a per-instance Tokio runtime), `AsyncClient` (via `pyo3-async-runtimes`), typed exception hierarchy |
| `synology_filestation/__init__.py` | Re-exports the sync surface (`Client`, exceptions) |
| `synology_filestation/aio.py`      | Re-exports `AsyncClient` |
| `synology_filestation/fsspec.py`   | `SynologyFileSystem(AsyncFileSystem)` — fsspec backend registered as protocol `synofs` (opt-in via `pip install synology-filestation[fsspec]`) |
| `tests/`                           | pytest + pytest-httpserver — covers atomic download, SID-expiry recovery, exception mapping, fsspec sync+async surface |

### FFI bindings (`rust/synology-filestation-ffi/`)

| File | Role |
|------|------|
| `src/lib.rs`     | C ABI: opaque `SynoClient`/`SynoMount` handles over an `Arc<SynologyClient>` + per-client Tokio runtime; `syno_connect` (returns `OtpRequired` so the GUI prompts), browse (`syno_list_*`/`syno_get_info`), transfers (`syno_download_to`/`syno_upload` with a progress callback), `syno_delete`/`syno_create_folder`/`syno_rename`, `syno_mount`/`syno_unmount`. Errors come back as a typed `SynoError` (status + DSM code + message); every export wraps `catch_unwind` |
| `src/logging.rs` | Fans `tracing` events out to a registered C log callback (`syno_set_log_callback`) for the GUI log pane **and** to an optional log file (`syno_set_log_file`, backed by the fuse crate's `logfile`). The pane dies with the GUI exactly as a terminal dies with its window, so the file is the only copy that survives the lock-up worth reading. `syno_mount` refuses a mountpoint that would contain the active log file |

The `kind`/`dsm_code` error classification mirrors the PyO3 exception mapping. Download progress is fine-grained (loops the core's ranged `download`); upload progress is coarse (the core upload is single-shot). Built/clipped/tested in CI alongside the CLI and core.

### Request throttling (protecting the NAS)

The FileStation Download API is proxied `nginx → synoscgi`, a **shared per-request CGI backend for the whole appliance** — sized for a handful of large streams, not a task-per-file fan-out. Parallel downloads (not total volume) saturate it, and an inner retry storm compounds a blip into an outage. The core client therefore offers an **opt-in throttle** (`ThrottleConfig` + `SynologyClient::with_throttle`) that wraps only the transfer calls (`download`/`upload`):

- **Concurrency semaphore** (`max_concurrency`, default 4) — single-digit global cap.
- **Rate-limit belt** (`min_interval`, default 150 ms) — spaces request starts even at full concurrency.
- **Jittered exponential backoff** (`backoff_base`..`backoff_max`, 1s→60s full-jitter) on transient/degraded responses.
- **Bounded per-file retries** (`max_attempts`, default 5) — then the error surfaces; no unbounded inner loop.
- **Error classification:** HTTP 502/503/504, 407 (backend fail-closing), connection/read errors, and DSM 402 (busy, backed off *harder*) → transient/retry; missing-file / no-permission / invalid-arg / any other DSM code → fail fast, no retry.

Who enables it:

| Consumer | Throttle | Rationale |
|---|---|---|
| Python `Client`/`AsyncClient` | **on by default** (all levers, incl. the belt); tunable/disable via `login(..., throttle=…, max_concurrency=…, …)` | The bulk consumer (e.g. a Temporal pipeline staging `.ORF` files) — the one that saturated the NAS. |
| FFI `syno_connect` (GUI) | **on**, concurrency + backoff + retry cap, **belt off** (`min_interval=0`) | The same client Arc also backs a GUI-initiated mount; spacing every ranged block read would stall interactive streaming. |
| FUSE/CLI (`main.rs`) | **off** | Interactive mount; streaming must stay responsive. The prefetch fan-out — the thing that actually made this path the highest-concurrency consumer — is bounded separately by `MAX_INFLIGHT_PREFETCH_BLOCKS`. |

**Temporal / outer-retry contract:** this client caps retries and then raises — do **not** nest it under your own inner retry loop. Let the activity fail and let Temporal reschedule with its (longer, jittered) backoff. Two nested retry loops are what produced the 200–250×-per-file storm.

**Structural fix for bulk raw staging:** prefer **SMB/NFS** on a mounted share over the HTTP Download API — it bypasses `synoscgi` entirely (no CGI backend to saturate). The UCSD campus firewall already permits SMB/NFS. Treat the HTTP Download path as the fallback; the throttle is the safety net for when it must be used. (See `python/synology_filestation/README.md` → *Throttling & reliability* / *Bulk staging*.)

### .NET GUI (`SynologyFuse.Gui/`)

MVVM pattern (Avalonia). The GUI calls the Rust core **directly via the FFI cdylib** (no subprocess):
- `Interop/NativeMethods.cs` — `[LibraryImport]` P/Invoke declarations (UTF-8 strings, blittable `NativeError`) + a `NativeLibrary` resolver that finds `lib*synology_filestation_ffi*` beside the GUI, in `target/{release,debug}`, or via the `SYNOFS_NATIVE_DIR` override.
- `Services/SynoClient.cs` — managed wrapper translating status codes to `SynoException`/`OtpRequiredException` and JSON to `SynoFileInfo`.
- `Services/MountService.cs` — connect + mount + test orchestration; surfaces native log lines via `OutputReceived`. A failure *after* a successful login is retagged `MountFailedException` so the UI advises on the mount point, not the credentials.
- `Services/ErrorPresenter.cs` — the last step of the error path: maps a `SynoException`'s `SynoStatus` (and, for a rejected login, the raw **`SYNO.API.Auth`** code — a different table from FileStation's, which is why the native layer reports `LoginFailed` separately) to an `ErrorReport` of *title / remedy / detail*. A pure function, so the wording is unit-tested without a NAS or the cdylib.
- `ViewModels/MainWindowViewModel.cs` — `IsConnecting` spinner, **Test Connection**, typed OTP retry (login once, reused for the mount), and `Report(ex)` → the `ErrorBannerViewModel` banner (cause + remedy + copyable raw detail). Errors used to reach the user as the word "Error" plus a log line, which never said what to fix.
- `ViewModels/FileBrowserViewModel.cs` + `Views/FileBrowserWindow.axaml` — pre-mount NAS browser (list/download/upload/delete/mkdir) with transfer progress.

`SettingsService.cs` persists config to platform-specific app-data directories.

### Caching (Linux Only)

- **`InodeCache`**: inode↔path bidirectional map with 30 s TTL (configurable via `--cache-ttl`)
- **`ReadCache`**: fixed-size block cache (default 256 MiB, `--read-cache-mb`)
- **Waiting on an in-flight block**: readers sleep on a per-block condvar rather than polling, and every path that claims a block releases the claim on the way out — a panic or an aborted prefetch task included, via `InflightGuard`. So an unresolved claim means a download still running, and `BLOCK_WAIT_TIMEOUT` giving up **leaves the claim alone**. It used to free it, on the theory that the owner must be dead; a timer cannot tell a dead owner from a slow one, so under load the mount freed live claims, a second reader restarted the same download, and the duplicated work lengthened the queue that caused the timeout
- **Prefetch** (`--prefetch-blocks`, default 16, `0` disables): speculation is no longer unconditional. `open` fetches block 0 eagerly and the full head-plus-tail media window **only** when block 0 sniffs as a container that keeps its index at the end (MP4/MOV, Matroska/WebM, AVI, ASF) — a seek to EOF is by definition not sequential, so no access-pattern heuristic could ever predict that tail, which is why it has to be decided from the file itself. Everything else gets block 0 plus a ramp: `read` prefetches only when a read continues the last one on that handle, growing 2→4→8→16. Blocks past EOF are never requested, and `release` aborts a file's outstanding prefetch once its last handle closes. Contiguous blocks are coalesced into a single ranged read (`MAX_PREFETCH_SPAN`, 16), so a fifteen-block head is one request rather than fifteen. Speculation is capped at `MAX_INFLIGHT_PREFETCH_BLOCKS` (32) **blocks** on the wire, separately from `MAX_CONCURRENT_TRANSFERS` (4) — wide enough for one file's whole open window (`depth - 1` head + 4 tail) or a video open serialises into waves. Counting blocks rather than requests is the point: the cap was written when a task was one block, and coalescing silently made the same number permit sixteen times the bytes. The budget is taken **before** the blocks are claimed, and a run that cannot have it is dropped rather than queued — a queued run holds claims, and a reader that wants one of those blocks then sleeps out `BLOCK_WAIT_TIMEOUT` behind a download that never started

### SMB read path

The `smb2` crate is built for concurrent, pipelined use: `Connection` is an `Arc` over shared state with its own credit accounting and receiver task, and `FileReader::read_at` is `pread`-shaped and explicitly safe to call concurrently. The transport wrapper originally defeated both — it held one mutex across every read and issued a fresh CREATE/READ/CLOSE per 256 KiB block, so every block on the whole mount was three round trips, serialised. Now:

- **The transport mutex is never held across a round trip.** It covers `ensure_ready` and cheap `Arc` clones of the connection and tree; the CREATE and the READ happen unlocked.
- **Open read handles are cached** (`smb/src/handles.rs`, `MAX_CACHED_HANDLES`), so a block read is one READ on a handle that persists. Invalidated on write/truncate/delete/rename/`open_write`, and dropped wholesale on reconnect (the session those handles belonged to is gone).
- **Closing is deferred, never skipped.** `FileReader::close` consumes the reader, and dropping one without closing leaks the handle on the appliance until session teardown — so an eviction that lands mid-read parks the handle until the last reader is done.
- **A dead link is rebuilt, on either leg.** `smb2` does not auto-reconnect, so `ReconnectState` flags a connection-lost error and the next operation re-establishes the session in `ensure_ready`. The direct leg lets `SmbClient` redial its own address. The tunnelled leg cannot — the NAS's in-tunnel address is reachable only from inside the tunnel, and `SmbClient` would dial it behind the caller's back — so `SmbTransport::over_with_redial` takes a closure that reopens the stream (`reopen_through` in the fuse crate, which calls `Chain::reach_smb` and so brings the tunnel back up if that is what is missing) and the session is rebuilt on it by the same `client_over` path that built the first one. A transport built with plain `SmbTransport::over` still cannot come back: without that closure one stream EOF ended SMB for the life of the mount, the transport-selection breaker re-probed the same dead session every 30 s, and every operation paid a guaranteed failure before falling back to HTTP A redial that comes back `Direct` — the laptop is back on the network — is dialled here too, so a mount that came up through a tunnel goes on working over TCP without a remount.

## Known Limitations

- Cross-directory directory moves are not supported
- Cross-directory file moves are not atomic (download → upload → delete)
- Write buffers are held entirely in memory until file close
- No symlinks, hard links, or special files
- Linux: inode numbers are not stable; permissions/timestamps are read-only
- Linux: ownership and mode are **synthetic** — every entry is reported as owned by the mounting user (`--uid`/`--gid`) with a `--umask`-derived mode, because DSM's uids and POSIX bits describe accounts on the appliance, not on this machine. Exporting them verbatim made GIO report `access::can-delete: FALSE` (see `fs::Ownership`). What the account may actually do is still enforced by DSM per call

## Release Process

Uses [release-please](https://github.com/googleapis/release-please-action) on the `main` branch. Commit messages follow **Conventional Commits**: while the project is pre-1.0, `fix:` → patch, `feat:` → minor, and `feat!:` → minor (`bump-minor-pre-major` holds 1.0.0 back until we mean it). `bump-patch-for-minor-pre-major` is deliberately **off** — leaving it on demoted every feature to a patch bump, which is how a release full of `feat:` commits came out as 0.3.2 instead of 0.4.0.

**Merge commits and the changelog: give pull requests prose titles.** A PR title must **not** carry a conventional-commit prefix — write "Keep the log after the machine goes down", not "feat(fuse): keep the log after the machine goes down". The commits inside the branch carry the prefixes; the PR title must not.

PRs land as merge commits, so the branch's own commits reach `main` alongside the merge — and GitHub puts the PR title into the merge commit. release-please parses that *and* the underlying commit, so a conventionally-titled PR makes every one of its entries appear **twice** in the changelog. A prose title parses as nothing, which is the whole trick: the merge commit contributes no entry and the changelog comes from the individual commits.

This cannot be fixed with a repository setting, though this file long claimed it was. GitHub now accepts only three title/message combinations — `PR_TITLE`+`PR_BODY`, `PR_TITLE`+`BLANK`, and `MERGE_MESSAGE`+`PR_TITLE` (what the repo is set to) — and every one of them puts the PR title somewhere release-please reads. `MERGE_MESSAGE`+`BLANK`, the combination that *would* have worked and that this file described as configured, is rejected with `invalid_merge_commit_setting_combo`. Checked 2026-09-14, after 0.7.0 shipped with every entry duplicated.

Release-please logs `commit could not be parsed` for each merge subject (`Merge pull request #N from …`). That noise is expected and is the system working.

The repo has **four release-please packages** linked together via the `linked-versions` plugin (so they always bump to the same version, but each ships its own changelog and tag):

| Package | Tag prefix | Artifacts on release |
|---|---|---|
| `.` (GUI + installers)            | `synology-filestation-gui-v...`   | (changelog + `SynologyFuse.Gui.csproj` `<Version>` bump) |
| `rust/synology-filestation-core`  | `synology-filestation-core-v...`  | (Cargo.toml bump only) |
| `rust/synology-filestation-fuse`  | `synology-filestation-fuse-v...`  | `.deb`, `.pkg`, `*-Setup.exe` (the MSI is built but ships inside the bundle, not as its own asset — checked against 0.5.5, 0.6.0 and 0.7.0) |
| `python/synology_filestation`     | `synology_filestation-v...`       | `*.whl` (manylinux 2_34, x86_64) |

**Why a root package exists.** release-please attributes a commit to a package by *path*, and the .NET projects (`SynologyFuse.Gui/`, `SynologyFuse.Tests/`, `SynologyFuse.*Installer/`) live at the repo root — outside every `rust/` and `python/` package. Without a package keyed `"."` those commits are "homeless": no package sees them, so no release PR is opened at all. (`include-paths` is *not* a release-please option — only `exclude-paths` is — so an earlier attempt to attribute them to the fuse package was a silent no-op.) The root package is given all commits and then filtered by `exclude-paths`, which lists **the paths another package already owns** — `rust/synology-filestation-core`, `rust/synology-filestation-fuse`, `python/synology_filestation` — plus `.github/` and `nix/`, which should not bump anything. It deliberately does *not* exclude `rust/` wholesale: the crates with no package of their own (`smb`, `connect`, `openvpn`, `ffi`) ship inside the released binary, and excluding their whole tree made every commit to them homeless — no changelog line, no version bump, for code that goes out in the artifact. Since it is in the linked-versions group, a GUI-only change still bumps the fuse package and ships the installers. Note `exclude-paths` matches directories only, so a commit touching just a root-level *file* (`flake.nix`, `README.md`) lands in the GUI package; only `feat`/`fix` types actually trigger a bump. `extra-files` paths are resolved **relative to the package path**, which is why the `SynologyFuse.Gui.csproj` version bump belongs to the root package, not the fuse package.

CI workflows:

| File | Trigger | Purpose |
|---|---|---|
| `.github/workflows/rust.yml`           | every push / PR        | clippy, build, test for the rust crates + .deb/.pkg/.msi smoke builds |
| `.github/workflows/python.yml`         | every push             | pytest matrix (Python 3.10–3.13) for the bindings |
| `.github/workflows/maturin.yml`        | every push             | manylinux wheel smoke build |
| `.github/workflows/nix.yml`            | push to main / PR      | `nix flake check` — CLI + GUI builds, clippy, rustfmt, GUI tests |
| `.github/workflows/release-please.yml` | tag (or workflow_dispatch) | release-please PRs and per-package artifact uploads |
