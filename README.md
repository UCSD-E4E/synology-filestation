# synology-filestation

Tools for working with Synology [FileStation](https://www.synology.com/en-global/dsm/feature/file_station) from outside DSM. The repo ships two related products from a single shared HTTP-client core:

**Filesystem driver** — mount a FileStation share as a local directory:
- **Linux** — FUSE via the `fuser` crate
- **macOS** — local WebDAV proxy; no kernel extension required
- **Windows** — user-mode filesystem via [WinFsp](https://winfsp.dev/)
- **GUI (all platforms)** — Avalonia desktop app for point-and-click mounting; calls the Rust core in-process (no subprocess), with a connecting spinner, a **Test Connection** check, and a pre-mount file browser

**Python package** (`pip install synology-filestation`) — typed exceptions, atomic downloads, transparent SID-expiry recovery; a drop-in replacement for the `synology-api` PyPI package's FileStation surface. Includes an [fsspec](https://filesystem-spec.readthedocs.io/) backend registered as protocol `synofs` so it composes with pandas / dask / polars / pyarrow. See [python/synology_filestation/README.md](python/synology_filestation/README.md).

## Features

- Browse and navigate directories
- Read and write files
- Create and delete files and directories
- Rename files (same-directory) and move files (cross-directory)
- Interactive password prompt with hidden input when password is not set via flag or environment variable
- 2FA / TOTP support
- Uses `rustls` — no OpenSSL dependency required
- **Linux:** metadata cache with configurable TTL; block-level read cache (default 256 MiB) with background prefetch
- **macOS:** no kernel extension; uses macOS's built-in WebDAV filesystem support
- **Windows:** user-mode filesystem via WinFsp; mounts as a drive letter (e.g. `Z:`)
- **GUI (all platforms):** Avalonia app that calls the Rust core directly through a native library (no subprocess) — connecting spinner, **Test Connection**, typed error messages, inline 2FA prompt, live log output, a pre-mount file browser (browse / download / upload / delete / new folder with transfer progress), and settings persistence

## Linux installation

Download the `.deb` package from the [Releases](../../releases) page:

```
synologyfuse-<version>_amd64.deb
```

Install with `apt` (which also satisfies the `libfuse3-3` dependency automatically):

```bash
sudo apt install ./synologyfuse-<version>_amd64.deb
```

This places the CLI at `/usr/bin/synology-filestation-fuse`, the GUI and its .NET runtime (plus the native `libsynology_filestation_ffi.so` the GUI loads) at `/opt/SynologyFuse/`, and a desktop launcher in your application menu.

To build the package yourself, see [Building the installer](#building-the-installer) below.

## macOS installation

Download the `.pkg` installer from the [Releases](../../releases) page:

```
SynologyFuse-<version>.pkg
```

The installer places `SynologyFuse.app` in `/Applications/` and symlinks the CLI to `/usr/local/bin/synology-filestation-fuse`. Administrator privileges are required. macOS 12 (Monterey) or later is required.

> **Gatekeeper note:** Because the package is currently unsigned, macOS may block it on first open. Right-click the `.pkg` and choose **Open**, then confirm in the dialog.

To build the installer yourself, see [Building the installer](#building-the-installer) below.

## Windows installation

The easiest way to install on Windows is to download the bundle installer from the [Releases](../../releases) page:

```
SynologyFuse-<version>-Setup.exe
```

The installer includes [WinFsp](https://winfsp.dev/) and automatically installs it if it is not already present, then installs SynologyFuse to `%ProgramFiles%\SynologyFuse\` and adds it to the system `PATH`. A **SynologyFuse GUI** shortcut is placed in the Start Menu.

To build the installer yourself, see [Building the installer](#building-the-installer) below.

## Nix installation

The repository ships a flake exposing both binaries as packages:

| Output | Installs the command | Contents |
|---|---|---|
| `synology-filestation-fuse` (also `default`) | `synology-filestation-fuse` | The Rust CLI |
| `synology-filestation-ffi` | *(library only)* | The native C ABI library (cdylib) the GUI loads |
| `synologyfuse-gui` | **`SynologyFuse.Gui`** | The .NET 10 / Avalonia desktop GUI |

Note the GUI's package name and its command differ: you install `synologyfuse-gui` but you run `SynologyFuse.Gui`.

The GUI calls the Rust core directly through the native library, so the `synologyfuse-gui` package wraps its launcher with `SYNOFS_NATIVE_DIR` pointing at the cdylib (and the CLI on `PATH` too) — installing or running the GUI alone is enough to mount. Install `synology-filestation-fuse` as well if you also want the CLI available directly in your shell.

### Try it without installing

```bash
nix run github:UCSD-E4E/synology-filestation          # CLI
nix run github:UCSD-E4E/synology-filestation#gui       # GUI (its wrapper puts the CLI on PATH)
```

### Imperative (user profile)

```bash
# GUI (self-contained), plus the CLI for direct shell use:
nix profile install \
  github:UCSD-E4E/synology-filestation#synologyfuse-gui \
  github:UCSD-E4E/synology-filestation#synology-filestation-fuse

SynologyFuse.Gui            # launch the GUI
synology-filestation-fuse   # …or the CLI
```

### Declarative (NixOS flake)

Add the flake as an input and pull the packages into `environment.systemPackages`:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    synology-filestation.url = "github:UCSD-E4E/synology-filestation";
    # Reuse your system's nixpkgs instead of evaluating a second copy:
    synology-filestation.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, synology-filestation, ... }: {
    nixosConfigurations.YOUR_HOST = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        ./configuration.nix
        ({ pkgs, ... }: {
          environment.systemPackages = [
            synology-filestation.packages.${pkgs.system}.synology-filestation-fuse
            synology-filestation.packages.${pkgs.system}.synologyfuse-gui   # optional
          ];

          # Required for non-root FUSE mounting (see below).
          programs.fuse.userAllowOther = true;
        })
      ];
    };
  };
}
```

Then `sudo nixos-rebuild switch --flake .#YOUR_HOST`. For [Home Manager](https://nix-community.github.io/home-manager/), use the same packages in `home.packages` instead.

On NixOS the CLI mounts via a setuid `fusermount3`; if you hit `fusermount3: permission denied`, the `programs.fuse.userAllowOther = true` option above provides it (this is the NixOS equivalent of the [`/etc/fuse.conf` step](#linux-fuse-configuration-allow_other) below).

### Develop against the flake

`nix develop` drops you into a shell with the Rust toolchain (clippy/rustfmt), `pkg-config` + `fuse3`, the Python binding toolchain (`uv`, `maturin`, `python3`), and the .NET 10 SDK — everything needed to build every component in this repo. `nix flake check` runs the CLI build, clippy, rustfmt, and the GUI build + test suite.

Supported systems: `x86_64-linux`, `aarch64-linux`, `x86_64-darwin`, `aarch64-darwin`.

## Requirements

### System

- **Linux:** kernel with FUSE support (`/dev/fuse`) and `libfuse3` runtime (`libfuse3-3` on Ubuntu/Debian)
- **macOS:** macOS 10.15+ (no third-party kernel extension needed)
- **Windows:** [WinFsp](https://winfsp.dev/rel/) installed (free, open-source); the `WinFsp.Launcher` service must be running
- A Synology NAS running DSM with FileStation API enabled

### Build

- Rust toolchain (1.70+) — install via [rustup](https://rustup.rs)
- **Linux only:** `libfuse3-dev` (Ubuntu/Debian) or `fuse3-devel` (Fedora/RHEL)
- **Windows only:** [WinFsp](https://winfsp.dev/rel/) installed (the build links against `winfsp-x64.dll`)
- **GUI (all platforms):** [.NET 10 SDK](https://dotnet.microsoft.com/download)
- **macOS installer only:** Xcode Command Line Tools — provides `pkgbuild`, `productbuild`, and `iconutil` (`xcode-select --install`)
- **Windows installer only:** `wix` dotnet tool (`dotnet tool install --global wix`)

```bash
# Ubuntu / Debian
sudo apt-get install libfuse3-dev

# Fedora / RHEL
sudo dnf install fuse3-devel
```

macOS requires no additional build dependencies.

**Windows note:** build from a **Developer Command Prompt**, **Developer PowerShell**, or any terminal where `vcvarsall.bat` has been sourced. Git Bash ships its own `link.exe` (a hard-link utility) that shadows MSVC's linker and will cause a build failure. If you must use Git Bash, set the linker explicitly via the environment variable:

```bash
export CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_LINKER="C:/Program Files/Microsoft Visual Studio/.../link.exe"
```

### Linux FUSE configuration (allow_other)

By default, a FUSE mount is only accessible to the user who created it.
The driver attempts to enable `allow_other` so that other users (and root) can
also access the mount. On Ubuntu 24.04 and other distributions that ship
`fusermount3`, this requires `user_allow_other` to be set in `/etc/fuse.conf`.

If that option is not present, the driver falls back to mounting **without**
`allow_other` and logs a warning — the mount still works for the mounting user.

To enable access for all users, edit `/etc/fuse.conf` and uncomment (or add) the
`user_allow_other` line:

```bash
# /etc/fuse.conf
# Allow non-root users to specify the allow_other or allow_root mount options
user_allow_other
```

You can do this in one command:

```bash
# If the line exists but is commented out, uncomment it;
# otherwise append it (handles fresh installs where the line is absent).
if grep -qE '^[[:space:]]*user_allow_other' /etc/fuse.conf 2>/dev/null; then
    : # already enabled — nothing to do
elif grep -qE '^#[[:space:]]*user_allow_other' /etc/fuse.conf 2>/dev/null; then
    sudo sed -i 's/^#[[:space:]]*user_allow_other/user_allow_other/' /etc/fuse.conf
else
    echo 'user_allow_other' | sudo tee -a /etc/fuse.conf
fi
```

After saving the file, remount and all users will be able to access the share.

**On NixOS**, `/etc/fuse.conf` is generated and the commands above will not
stick. Set the option instead:

```nix
programs.fuse.userAllowOther = true;
```

## Building

### CLI (all platforms)

```bash
cargo build --release -p synology-filestation-fuse
```

The binary will be at `target/release/synology-filestation-fuse`.

The repo is a Cargo workspace; `-p synology-filestation-fuse` keeps the build scoped to the FUSE binary and its `synology-filestation-core` library dependency. Building the whole workspace also pulls in [`python/synology_filestation/`](python/synology_filestation/), which requires Python development headers — see [the Python package's README](python/synology_filestation/README.md) for that.

### GUI (all platforms)

The GUI is a self-contained .NET 10 / Avalonia application. Build it for any platform with:

```bash
# Linux (x64)
dotnet publish SynologyFuse.Gui -c Release -r linux-x64 -p:SelfContained=true

# macOS (Apple Silicon)
dotnet publish SynologyFuse.Gui -c Release -r osx-arm64 -p:SelfContained=true

# macOS (Intel)
dotnet publish SynologyFuse.Gui -c Release -r osx-x64 -p:SelfContained=true

# Windows (x64)
dotnet publish SynologyFuse.Gui -c Release -r win-x64 -p:SelfContained=true
```

Output goes to `SynologyFuse.Gui/bin/Release/net10.0/<rid>/publish/`. The GUI calls the Rust core through the native FFI library (`libsynology_filestation_ffi.{so,dylib}` / `synology_filestation_ffi.dll`); build it with `cargo build --release -p synology-filestation-ffi` and place it beside the GUI executable. The resolver searches, in order: the `SYNOFS_NATIVE_DIR` environment variable (explicit override), then beside the GUI (deployed layout), then `target/{release,debug}/` relative to the repo root (development layout). The platform installers bundle the library automatically.

### Building the installer

#### Linux (Debian/Ubuntu .deb)

**Prerequisites (one-time):**

```bash
sudo apt-get install dpkg-dev
```

**Build (all-in-one):**

```bash
./SynologyFuse.DebInstaller/Build-Package.sh --build
# Produces: SynologyFuse.DebInstaller/synologyfuse-0.1.2_amd64.deb
```

Pass `--build` to compile the Rust CLI and publish the .NET GUI automatically. Omit it if you have already run `cargo build --release -p synology-filestation-fuse` and `dotnet publish` yourself. Use `--arch arm64` to target 64-bit ARM instead of x86-64 (default: `amd64`).

The package installs:

| Path | Contents |
|---|---|
| `/usr/bin/synology-filestation-fuse` | CLI binary |
| `/opt/SynologyFuse/` | GUI executable, .NET runtime, and `libsynology_filestation_ffi.so` |
| `/usr/share/applications/synologyfuse.desktop` | Application menu entry |
| `/usr/share/icons/hicolor/256x256/apps/synologyfuse.png` | Application icon |

Runtime dependency: `libfuse3-3` (pulled in automatically by `apt install`).

#### macOS

**Prerequisites (one-time):**

```bash
xcode-select --install   # provides pkgbuild, productbuild, iconutil
```

**Build (all-in-one):**

```bash
./SynologyFuse.MacInstaller/Build-Installer.sh --build
# Produces: SynologyFuse.MacInstaller/SynologyFuse-0.1.2.pkg
```

Pass `--build` to compile the Rust CLI and publish the .NET GUI automatically. Omit it if you have already run `cargo build --release -p synology-filestation-fuse` and `dotnet publish` yourself. Use `--arch x86_64` to target Intel Macs instead of Apple Silicon (default: `arm64`).

#### Windows

Two Windows installers are available:

| File | What it contains | Use when |
|---|---|---|
| `SynologyFuse-<ver>.msi` | SynologyFuse only | WinFsp is already installed |
| `SynologyFuse-<ver>-Setup.exe` | WinFsp + SynologyFuse | Fresh Windows machine |

**Prerequisites (one-time):**

```powershell
dotnet tool install --global wix
wix extension add WixToolset.Bal.wixext
wix extension add WixToolset.Util.wixext
```

**Build the MSI only:**

```powershell
# From repo root — build Rust CLI and .NET GUI first
cargo build -r -p synology-filestation-fuse
dotnet publish SynologyFuse.Gui -c Release -r win-x64 -p:SelfContained=true

msbuild SynologyFuse.Installer\SynologyFuse.Installer.wixproj
# Produces: SynologyFuse.Installer\SynologyFuse-0.1.2.msi
```

**Build the bundle (MSI + WinFsp):**

```powershell
# Build Rust CLI and .NET GUI first (same as above), then:
msbuild SynologyFuse.Installer\SynologyFuse.Installer.wixproj
.\SynologyFuse.Installer\Build-Bundle.ps1
# Downloads WinFsp automatically, then produces:
# SynologyFuse.Installer\SynologyFuse-0.1.2-Setup.exe
```

The bundle script downloads `winfsp-2.1.25156.msi` from GitHub to `SynologyFuse.Installer\redist\` (cached for subsequent builds).

## Usage

```bash
synology-filestation-fuse --host <NAS_HOST> -u <USERNAME> [OPTIONS] <MOUNTPOINT>
```

### Arguments

| Argument | Description | Default |
|---|---|---|
| `<MOUNTPOINT>` | Directory to mount on. On macOS must be under `/Volumes/` (e.g. `/Volumes/nas`). On Windows must be an empty directory (e.g. `C:\mnt\nas`) | *(required)* |
| `--host <HOST>` | NAS hostname or IP address | *(required)* |
| `-u, --username` | NAS account username | *(required)* |
| `-p, --password` | NAS account password (or `SYNO_PASSWORD` env var; prompted with hidden input if omitted) | *(optional)* |
| `--otp <CODE>` | TOTP code for 2FA (or `SYNO_OTP` env var); prompted interactively if omitted and 2FA is enabled | *(optional)* |
| `--port <PORT>` | API port | `5001` |
| `--https` | Use HTTPS. Always on: the flag takes no value and there is no `--no-https`, so plain HTTP is not currently reachable from the CLI | `true` |
| `--password-stdin` | Read the password from the first line of stdin. Prefer this in scripts: `--password` puts the password in argv, where any local account can read it with `ps`. | `false` |
| `--insecure` | Accept any TLS certificate (self-signed, expired, wrong hostname). Needed for a stock DSM certificate; the connection is then encrypted but **not** authenticated. Env: `SYNO_INSECURE` | `false` |
| `--cache-ttl <SECS>` | Metadata cache TTL in seconds *(Linux only)* | `30` |
| `--read-cache-mb <MiB>` | Read cache size in MiB *(Linux only)* | `256` |
| `--prefetch-blocks <N>` | Speculative read-ahead depth in 256 KiB blocks; `0` disables it. Read-ahead only fires for a reader that is streaming, and the window at open only for a container that keeps its index at the end. A bulk walk over a corpus reads each file once, so it wants `0` *(Linux only)* | `16` |
| `--uid <UID>` | Owner reported for every mounted entry. DSM's own uids name accounts on the appliance, not on this machine, so they are never used *(Linux only)* | *(mounting user)* |
| `--gid <GID>` | Group reported for every mounted entry *(Linux only)* | *(mounting user's group)* |
| `--umask <MASK>` | Octal umask for the permissions the mount reports; `022` gives `0755` directories and `0644` files *(Linux only)* | `022` |
| `--log-level <LEVEL>` | Log level (`error`, `warn`, `info`, `debug`, `trace`) | `info` |
| `--log-file <PATH>` | Also write the log to this file, so it outlives the terminal. Records reach the OS as they are emitted, and warnings and errors are forced to disk, so a killed or power-cycled process still leaves them behind — which is the whole point: a mount bad enough to need the power button is the one worth reading afterwards. Refused if the path is inside the mountpoint (the log would be written through the mount it describes, deadlocking it). Rotates at 8 MiB, keeping 3 older generations beside it | *(none)* |
| `--fuse-threads <N>` | FUSE event-loop threads; `0` picks a default from the CPU count. Bounds the callbacks that still hold a thread (a cache-missing read, a listing, a metadata call) — not file transfers, which run on the async runtime *(Linux only)* | `0` |

Transport selection — see [Transports](#transports) for how these combine:

| Argument | Description | Default |
|---|---|---|
| `--domain <DOMAIN>` | NetBIOS domain the account lives in (e.g. `KRG` for an AD user; omit for a local DSM account). Used by both legs that authenticate against the directory: SMB, and the VPN, whose DSM front end refuses a name that does not carry it. Env: `SYNOLOGY_FS_SMB_DOMAIN` | *(none)* |
| `--vpn-profile <PATH>` | The OpenVPN profile **on this computer**. Supplying it is what makes the tunnel leg exist at all — without it the chain falls from direct SMB straight to HTTP. Used as-is if the file is present, otherwise fetched from `--vpn-profile-nas`. Written owner-only, since it embeds `ta.key` | *(none)* |
| `--vpn-profile-nas <PATH>` | The same profile's path **on the NAS**, to fetch it from over the session just authenticated. Has no effect unless `--vpn-profile` also says where to put it | *(none)* |
| `--vpn-host <ADDR>` | The NAS's address *inside* the tunnel. Required for the tunnel leg: the server pushes no DNS, so the NAS's public name does not resolve to it | *(none)* |
| `--disable-smb` | Never try SMB. Disabling means *not probing*, so on a network that black-holes port 445 this also saves the probe timeout. Implies `--disable-vpn`. Env: `SYNOLOGY_FS_SMB_DISABLE` (any value) | `false` |
| `--disable-vpn` | Never bring up the tunnel. Direct SMB is still tried, so a mount on a network where the NAS answers directly is unaffected | `false` |
| `--disable-https` | Never fall back to the HTTP FileStation API. With SMB unreachable the mount then fails loudly instead of quietly using a transport that cannot resume an interrupted transfer | `false` |

Environment variables with no flag of their own:

| Variable | Description | Default |
|---|---|---|
| `SYNOLOGY_FS_SMB_TIMEOUT_MS` | How long the direct-SMB probe waits for port 445 before escalating | `2000` |
| `SYNOLOGY_FS_SMB_PORT` | SMB port to dial | `445` |

### Transports

The mount does not always talk to the NAS the same way. At startup it walks a chain and keeps the first leg that answers:

| Leg | How | When it wins |
|---|---|---|
| **SMB, direct** | Port 445 straight to `--host` | On a network with a route to the NAS |
| **SMB, through a tunnel** | An OpenVPN tunnel this process raises itself — no `tun` device, no privileged helper, no effect on anything else the machine is doing — then SMB to `--vpn-host` inside it | Off the NAS's network, when a profile is configured |
| **HTTP FileStation API** | The same DSM port the login used | Last resort; the only leg that cannot resume an interrupted transfer |

SMB is the default and needs no flag: the chain probes it first and only falls onward when it does not answer. The `--disable-*` flags remove legs from consideration; nothing opts *in*.

**Reaching SMB from off the network** needs three things, and quietly does nothing if any is missing:

1. `--vpn-profile` — a local path for the `.ovpn`. **This is the flag that creates the tunnel leg.** Without it there is nothing to dial, and the chain falls from direct SMB straight to HTTP.
2. `--vpn-host` — the NAS's address inside the tunnel. The server pushes no DNS, so its public name does not resolve there and the probe has nowhere to land.
3. `--domain` — for an AD account. The VPN's DSM front end refuses a username that does not carry its domain.

`--vpn-profile-nas` on its own is **not** enough. It only names where to fetch the profile *from*; the fetch is skipped unless `--vpn-profile` says where to put it. A run missing it logs:

```
WARN synology_filestation_connect: transport: no SMB through the tunnel (no tunnel is configured)
INFO synology_filestation_connect: transport: falling back to the HTTP API
```

The first run downloads the profile over the session it has just authenticated — which is what lets somebody outside the NAS's network get the file that gets them inside it — and writes it readable only by its owner. Later runs use whatever is on disk, so `--vpn-profile-nas` can be dropped once the file is there.

The chosen leg is logged at startup as `Transport: …` — `SMB at <host>`, `SMB via VPN at <address>`, or `HTTPS`. A running mount keeps looking for a better leg and moves to it without a remount, logging `Transport: now SMB at <host>, was HTTPS` when it does. Pass `--disable-https` to make a silent fall back to the API impossible, so a mount that expected SMB fails instead of running slowly.

### Examples

```bash
# Mount — password will be prompted securely if not supplied
# Linux
mkdir -p /mnt/nas
synology-filestation-fuse --host 192.168.1.100 -u admin /mnt/nas

# macOS (mountpoint must be under /Volumes/)
synology-filestation-fuse --host 192.168.1.100 -u admin /Volumes/nas
# Password: ********

# Windows (mountpoint must be an empty directory)
synology-filestation-fuse.exe --host 192.168.1.100 -u admin C:\mnt\nas

# Pass password via flag
synology-filestation-fuse --host 192.168.1.100 -u admin -p mypassword /mnt/nas

# Pass password via environment variable
export SYNO_PASSWORD=mypassword
synology-filestation-fuse --host nas.local -u admin /mnt/nas

# With two-factor authentication (TOTP code passed directly)
synology-filestation-fuse --host 192.168.1.100 -u admin --otp 123456 /mnt/nas

# With 2FA via environment variable
SYNO_OTP=123456 synology-filestation-fuse --host 192.168.1.100 -u admin /mnt/nas

# With 2FA enabled but no code supplied — will prompt interactively:
#   Two-factor authentication code: ______
synology-filestation-fuse --host 192.168.1.100 -u admin /mnt/nas

# Linux: larger read cache for smoother playback of large video files
synology-filestation-fuse --host nas.local -u admin --read-cache-mb 512 /mnt/nas


# SMB where the NAS answers directly — nothing to pass; the chain probes 445 first.
# --domain is what an AD account needs; omit it for a local DSM account.
synology-filestation-fuse --host nas.example.edu -u alice --domain KRG /mnt/nas

# SMB from off the network, through a tunnel this process raises itself.
# All three of --vpn-profile / --vpn-host / --domain are needed; the profile is
# fetched from the NAS on the first run and reused from disk afterwards.
synology-filestation-fuse \
  --host nas.example.edu --port 6021 \
  -u alice --domain KRG \
  --vpn-host 10.90.24.1 \
  --vpn-profile ~/.config/synology-filestation/nas-vpn.ovpn \
  --vpn-profile-nas /installers/nas-vpn.ovpn \
  /mnt/nas

# Same, once the profile is on disk
synology-filestation-fuse --host nas.example.edu --port 6021 \
  -u alice --domain KRG --vpn-host 10.90.24.1 \
  --vpn-profile ~/.config/synology-filestation/nas-vpn.ovpn /mnt/nas

# Refuse to fall back to the HTTP API — fail loudly if SMB is unreachable
synology-filestation-fuse --host nas.example.edu -u alice --domain KRG \
  --vpn-host 10.90.24.1 --vpn-profile ~/.config/synology-filestation/nas-vpn.ovpn \
  --disable-https /mnt/nas

# Skip SMB entirely (also skips the 445 probe timeout, and implies --disable-vpn)
synology-filestation-fuse --host nas.example.edu -u alice --disable-smb /mnt/nas

# Shorten the direct-SMB probe on a network known to black-hole port 445
SYNOLOGY_FS_SMB_TIMEOUT_MS=500 \
  synology-filestation-fuse --host nas.example.edu -u alice /mnt/nas

# Enable debug logging
synology-filestation-fuse --host nas.local -u admin --log-level debug /mnt/nas

# Keep the log after the terminal is gone. Worth doing before any run you might
# have to explain afterwards — a mount that wedges the machine takes its
# scrollback with it, and this is what is left to read.
synology-filestation-fuse --host nas.local -u admin \
  --log-level debug --log-file ~/.local/state/synology-filestation/mount.log /mnt/nas

# Unmount (Linux)
fusermount -u /mnt/nas

# Unmount (macOS — or eject in Finder)
diskutil unmount /Volumes/nas

# Unmount (Windows — press Ctrl-C in the terminal running the driver,
# or right-click the drive in Explorer and choose Eject / Disconnect)
```

## Architecture

The repo is a Cargo workspace with four Rust crates plus the .NET desktop GUI and platform installers:

```
synology-filestation/
├── rust/
│   ├── synology-filestation-core/   Pure async HTTP client (no platform deps)
│   │   └── src/
│   │       ├── client.rs            FileStation API; auto-relogin; atomic download_to_path
│   │       ├── types.rs             Serde types for API JSON responses
│   │       └── error.rs             SynoFsError + Linux errno mapping
│   ├── synology-filestation-fuse/   CLI + FUSE / WebDAV / WinFsp backends
│   │   └── src/
│   │       ├── main.rs              CLI: arg parsing, Tokio runtime, login, then spawn_mount
│   │       ├── lib.rs               spawn_mount / MountHandle (non-blocking) — shared by CLI + FFI
│   │       ├── fs.rs                fuser::Filesystem trait (Linux FUSE backend)
│   │       ├── cache.rs             Inode ↔ path cache + block-level read cache (Linux)
│   │       ├── webdav.rs            dav_server::DavFileSystem trait (macOS WebDAV backend)
│   │       └── winfs.rs             winfsp::FileSystemContext trait (Windows WinFsp backend)
│   └── synology-filestation-ffi/    C ABI cdylib consumed by the GUI (P/Invoke)
│       └── src/
│           ├── lib.rs               connect / browse / transfer / mount; typed SynoError; catch_unwind
│           └── logging.rs           tracing → C log callback bridge
├── python/
│   └── synology_filestation/        PyO3 + maturin Python bindings (synofs fsspec)
│       ├── src/lib.rs               PyO3 _native module: Client, AsyncClient, exceptions
│       ├── synology_filestation/    Importable Python package (re-exports + fsspec.py)
│       └── tests/                   pytest + pytest-httpserver
├── SynologyFuse.Gui/                Cross-platform desktop GUI (Avalonia / .NET 10)
├── SynologyFuse.DebInstaller/       Linux .deb package (dpkg-deb)
├── SynologyFuse.MacInstaller/       macOS .pkg installer (pkgbuild / productbuild)
├── SynologyFuse.Installer/          Windows MSI + bundle installer (WiX 6)
└── SynologyFuse.Tests/              xUnit tests for the GUI project
```

### GUI

`SynologyFuse.Gui` is a cross-platform Avalonia application that calls the Rust core **in-process** through the `synology-filestation-ffi` cdylib via P/Invoke — it does not spawn the CLI. It provides:

- Form fields for host, username, password, OTP, mountpoint, and advanced options (cache TTL, read cache size, log level)
- **Connect** (login + mount), **Test Connection** (login only), and **Disconnect** (unmount + logout), with a connecting spinner while a call is in flight
- Typed error reporting — the native layer returns structured errors (with the DSM code), so messages are accurate instead of scraped from log text
- Inline 2FA prompt: when login reports that an OTP is required, the GUI shows an input banner and retries the connect with the code (entered once, then reused for the mount)
- A pre-mount **file browser** ([`FileBrowserWindow`](SynologyFuse.Gui/Views/FileBrowserWindow.axaml)) that lists shares/directories and supports download, upload, delete, and new-folder with a transfer progress bar — all without mounting
- Live log output, bridged from the native library's `tracing` events via a log callback
- Settings persistence — connection parameters are saved to the platform settings directory and reloaded on next launch

The P/Invoke surface lives in `SynologyFuse.Gui/Interop/` (`NativeMethods.cs` declarations + native-library resolver, `SynoException.cs`); `Services/SynoClient.cs` is the managed wrapper and `Services/MountService.cs` orchestrates connect/mount/test.

### Virtual root

The mountpoint root is a synthetic directory that does not correspond to any real path on the NAS. Reading it calls `SYNO.FileStation.List list_share` to enumerate all shares the authenticated account can see. Each share appears as a subdirectory.

### macOS WebDAV backend

On macOS the binary starts a local HTTP server (`127.0.0.1:<random port>`) that speaks WebDAV, then calls the macOS built-in `mount_webdav` to attach it at the requested mountpoint. This uses Apple's own `webdavfs` kernel support — no third-party kernel extension is required. The process exits cleanly on Ctrl-C or when the volume is ejected from Finder.

WebDAV operations map to FileStation API calls:

| WebDAV | FileStation |
|---|---|
| `PROPFIND /` | `list_share` |
| `PROPFIND /path` | `getinfo` + `list` |
| `GET /path` (Range) | `download` |
| `PUT /path` | `upload` |
| `DELETE /path` | `delete` |
| `MKCOL /path` | `CreateFolder` |
| `MOVE` (same dir) | `rename` |
| `MOVE` (cross dir) | `download` → `upload` → `delete` |

### Windows WinFsp backend

On Windows the binary registers a user-mode filesystem with WinFsp and mounts it at the specified drive letter (e.g. `Z:`). WinFsp acts as a kernel-mode bridge: the Windows kernel forwards filesystem requests to our process via the WinFsp driver, which calls our `FileSystemContext` callbacks synchronously.

WinFsp callbacks map to FileStation API calls:

| WinFsp callback | FileStation |
|---|---|
| `get_security_by_name` | `getinfo` (or pending-file registry hit) |
| `open` | `getinfo` |
| `create` | *(deferred — file data buffered in memory)* |
| `read` | `download` (byte-range) |
| `write` | *(buffered in memory)* |
| `flush` / `close` | `upload` |
| `rename` (same dir, buffered) | `upload` direct to new name |
| `rename` (same dir, on NAS) | `rename` |
| `rename` (cross dir) | `download` → `upload` → `delete` |
| `cleanup` (delete flag) | `delete` |
| `read_directory` | `list_share` or `list` |
| `create` (directory) | `CreateFolder` |

Because the Synology Upload API requires the complete file body in a single multipart request, new and modified files are buffered in memory and uploaded atomically on `flush()`, `rename()`, or `close()`. During the write phase the file is tracked in an in-process pending-file registry so that concurrent opens (e.g. from an atomic write pattern) can locate the buffered data without a NAS round-trip.

Rename handling normalises the destination path case: Windows may supply the destination in a different case than the NAS directories (which are on a Linux, case-sensitive filesystem). The driver always uses the source path's parent (correct mixed-case) for uploads and same-directory renames.

### Linux FUSE backend

The FUSE dispatch loop (inside `fuser::mount2`) is synchronous. All Synology API calls are async via `reqwest`. The driver bridges these by holding a `tokio::runtime::Handle` and calling `handle.block_on(future)` inside each FUSE callback.

File data is cached in fixed-size 256 KiB blocks using a `moka` LRU cache (default 256 MiB). Background prefetch of the next 16 blocks keeps streaming reads smooth.

### Write buffering (all platforms)

The Synology Upload API requires the complete file body as a single multipart upload. Write data is accumulated in an in-memory buffer per open file handle and flushed to the NAS on close. Large file writes consume proportional memory.

## Known Limitations

- **Cross-directory directory moves are not supported.** Moving a directory to a different parent returns `ENOSYS` / `STATUS_UNSUCCESSFUL`. Only same-directory renames use the efficient `SYNO.FileStation.Rename` API.
- **Cross-directory file moves are not atomic.** They are implemented as download → upload → delete. A failure mid-sequence may leave data duplicated or missing.
- **Large files require large in-memory write buffers.** Writing to a file buffers the entire file content in process memory until the file is closed or flushed.
- **Overwriting a file is not atomic.** The existing file is deleted before the new content is uploaded; a crash between those two steps would result in data loss.
- **No support for symlinks, hard links, or special files.**
- **Linux only:** Inode numbers are not stable across remounts. Permissions and timestamps are read-only (`chmod`, `chown`, `utimens` appear to succeed but are not persisted).
- **Windows only:** The mountpoint must be an empty directory (e.g. `C:\mnt\nas`); mounting directly as a drive letter (e.g. `Z:`) is not currently supported.

## Synology API Reference

All requests go to `/webapi/entry.cgi` (authentication uses `/webapi/auth.cgi`).

| Operation | API | Method |
|---|---|---|
| Login | `SYNO.API.Auth` | `login` |
| Logout | `SYNO.API.Auth` | `logout` |
| List shares | `SYNO.FileStation.List` | `list_share` |
| List directory | `SYNO.FileStation.List` | `list` |
| Get file info | `SYNO.FileStation.List` | `getinfo` |
| Download file | `SYNO.FileStation.Download` | `download` |
| Upload file | `SYNO.FileStation.Upload` | `upload` |
| Delete | `SYNO.FileStation.Delete` | `delete` |
| Create folder | `SYNO.FileStation.CreateFolder` | `create` |
| Rename | `SYNO.FileStation.Rename` | `rename` |

## Contributing

This project uses [Conventional Commits](https://www.conventionalcommits.org/). Commit messages determine the next version number automatically via [release-please](https://github.com/googleapis/release-please):

| Commit prefix | Example | Version bump |
|---|---|---|
| `fix:` | `fix: handle HTTP 416 as EOF` | patch (`0.1.0` → `0.1.1`) |
| `feat:` | `feat: add read cache` | minor (`0.1.0` → `0.2.0`) |
| `feat!:` or `BREAKING CHANGE:` footer | `feat!: change CLI flag names` | minor while pre-1.0 (`0.3.0` → `0.4.0`) |

While the project is pre-1.0, `bump-minor-pre-major` keeps a breaking change on the minor track rather than reaching for `1.0.0`; the jump to `1.0.0` is a deliberate act, not something a `!` triggers.

Other prefixes (`docs:`, `chore:`, `refactor:`, `test:`, etc.) do not trigger a release.

When a qualifying commit lands on `main`, release-please opens a PR that bumps the version in `Cargo.toml` and updates `CHANGELOG.md`. Merging that PR creates a GitHub release and tag.

## License

MIT
