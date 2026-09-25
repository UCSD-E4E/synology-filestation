use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use synology_filestation_connect::{
    profile::ProfileSource, Chain, Endpoints, NoTransport, NoTunnel, OpenVpnTunnel, TcpProber,
    Transport, TransportPolicy, Tunnel, DEFAULT_RECHECK,
};
use synology_filestation_smb::SmbConfig;

use clap::Parser;
use tracing::{info, warn};

use synology_filestation_core::client::SynologyClient;
use synology_filestation_fuse::legs::{Legs, DEFAULT_WATCH_INTERVAL};
use synology_filestation_fuse::logfile::{self, LogFile};
use synology_filestation_fuse::{is_otp_required, spawn_mount, MountOptions};

/// Aborts a background task when it goes out of scope.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "synology-fuse",
    about = "Mount a Synology FileStation share as a local filesystem"
)]
struct Args {
    /// Synology NAS hostname or IP address
    #[arg(long)]
    host: String,

    /// HTTPS port (5001 by default; use 5000 for HTTP)
    #[arg(long, default_value_t = 5001)]
    port: u16,

    /// Use HTTPS (disable to use plain HTTP)
    #[arg(long, default_value_t = true)]
    https: bool,

    /// Accept any TLS certificate, including self-signed, expired, or
    /// wrong-hostname ones.
    ///
    /// A DSM appliance ships with a self-signed certificate, so this is often
    /// needed — but it means the encrypted connection is not authenticated:
    /// anything able to intercept it can present its own certificate and read
    /// your password. Prefer installing the NAS's certificate in the system
    /// trust store.
    #[arg(long, env = "SYNO_INSECURE")]
    insecure: bool,

    /// NAS account username
    #[arg(long, short = 'u')]
    username: String,

    /// NAS account password (or set SYNO_PASSWORD env var; prompted if omitted)
    #[arg(long, short = 'p', env = "SYNO_PASSWORD")]
    password: Option<String>,

    /// Read the password from the first line of stdin.
    ///
    /// Prefer this in scripts. A password passed as `--password` sits in this
    /// process's argv, which every other account on the machine can read via
    /// `ps` (and `/proc/<pid>/cmdline` on Linux) for as long as the mount runs.
    #[arg(long, conflicts_with = "password")]
    password_stdin: bool,
    /// TOTP code for two-factor authentication (or set SYNO_OTP env var).
    /// If 2FA is enabled and this is not provided, you will be prompted interactively.
    #[arg(long, env = "SYNO_OTP")]
    otp: Option<String>,

    /// Local directory to mount the filesystem on
    mountpoint: PathBuf,

    /// Metadata cache TTL in seconds (Linux/FUSE only)
    #[arg(long, default_value_t = 30)]
    cache_ttl: u64,

    /// Read cache size in MiB (Linux/FUSE only)
    #[arg(long, default_value_t = 256)]
    read_cache_mb: u64,

    /// Speculative read-ahead depth in 256 KiB blocks; 0 disables it.
    /// Read-ahead only fires for a reader that is streaming, and the window
    /// at open only for a container that keeps its index at the end. Bulk
    /// consumers walking a corpus want 0 (Linux/FUSE only)
    #[arg(long, default_value_t = synology_filestation_fuse::DEFAULT_PREFETCH_BLOCKS)]
    prefetch_blocks: u64,

    /// FUSE event-loop threads; 0 picks a default from the CPU count.
    /// Bounds the callbacks that still hold a thread — a read that misses the
    /// cache, a listing, a metadata call — not file transfers, which run on the
    /// async runtime (Linux/FUSE only)
    #[arg(long, default_value_t = 0)]
    fuse_threads: usize,

    /// Owner reported for every mounted entry; defaults to the mounting user.
    /// DSM's own uids name accounts on the appliance and are never used
    /// (Linux/FUSE only)
    #[arg(long)]
    uid: Option<u32>,

    /// Group reported for every mounted entry; defaults to the mounting user's
    /// group (Linux/FUSE only)
    #[arg(long)]
    gid: Option<u32>,

    /// Umask for the synthetic permissions the mount reports. The default 0o022
    /// gives 0755 directories and 0644 files (Linux/FUSE only)
    #[arg(long, value_parser = parse_umask, default_value = "022")]
    umask: u16,

    /// Log level (error, warn, info, debug, trace)
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Also write the log to this file, so it survives the terminal.
    ///
    /// Stderr goes away when the terminal does — including when a wedged mount
    /// forces a power cycle, which is exactly the run worth reading afterwards.
    /// Records reach the OS as they are emitted and warnings and errors are
    /// forced to disk, so a killed or powered-off process still leaves them.
    /// The file rotates at 8 MiB, keeping three older generations beside it.
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,

    /// Never try SMB. Disabling means *not probing*: on a network that
    /// black-holes port 445 this also saves the connect timeout. It implies
    /// `--disable-vpn`, since a tunnel exists to make SMB reachable
    #[arg(long)]
    disable_smb: bool,

    /// Never bring up the tunnel. SMB is still tried directly, so a mount on
    /// a network where the NAS answers directly is unaffected; elsewhere this
    /// falls straight to HTTPS with no
    /// tunnel prompt
    #[arg(long)]
    disable_vpn: bool,

    /// Never fall back to the HTTP FileStation API. With SMB unreachable the
    /// mount then fails loudly instead of quietly using a transport that
    /// cannot resume an interrupted transfer
    #[arg(long)]
    disable_https: bool,

    /// The NAS's address *inside* the tunnel, which its public name does not
    /// resolve to — an OpenVPN server that pushes no DNS is the ordinary case,
    /// and the address is whatever it hands out on its own subnet
    #[arg(long)]
    vpn_host: Option<String>,

    /// NetBIOS domain the account lives in — `KRG` for an AD user, omitted for
    /// a local DSM one. Used by both legs that authenticate against the
    /// directory: SMB, and the VPN, whose DSM front end refuses a name that
    /// does not carry it. Falls back to `SYNOLOGY_FS_SMB_DOMAIN` — the old
    /// name, kept because mounts in the field are configured with it
    #[arg(long, env = "SYNOLOGY_FS_SMB_DOMAIN")]
    domain: Option<String>,

    /// The OpenVPN profile on *this computer*.
    ///
    /// Given this, a NAS that does not answer directly is reached through a
    /// tunnel this process raises itself — no tun device, no privileged
    /// helper, and no effect on anything else the machine is doing.
    ///
    /// Used as-is if the file is there. If it is not, and `--vpn-profile-nas`
    /// says where to find it, it is fetched over the session authenticated
    /// below — which is what lets somebody outside the NAS's network get the
    /// file that gets them inside it.
    ///
    /// The file embeds `ta.key`, so it is a shared secret: it is written
    /// readable only by its owner, and never logged.
    #[arg(long)]
    vpn_profile: Option<PathBuf>,

    /// The same profile's path on *the NAS*, to fetch it from.
    ///
    /// No default: where an appliance keeps such a file is a decision whoever
    /// set it up made, and this client has no business assuming a layout.
    #[arg(long)]
    vpn_profile_nas: Option<String>,
}

/// How long the whole tunnel attempt may take, handshake included.
///
/// Only reached when SMB did not answer directly, and the alternative is the
/// HTTP leg — so this is the wait somebody pays once, before a decision that
/// is then remembered.
const VPN_PATIENCE: Duration = Duration::from_secs(30);

/// The tunnel these arguments describe, or one that never comes up.
///
/// A profile is what makes an escalation possible: without one there is
/// nothing to dial, nothing to authenticate against, and no address to be
/// given. Saying so with [`NoTunnel`] means the chain falls from SMB straight
/// to HTTP with nothing to wait for.
fn tunnel_from(args: &Args, username: &str, password: &str) -> Box<dyn Tunnel> {
    match &args.vpn_profile {
        Some(profile) => Box::new(OpenVpnTunnel::new(
            profile,
            username,
            password,
            args.domain.as_deref(),
            VPN_PATIENCE,
        )),
        None => Box::new(NoTunnel),
    }
}

/// Predates `--disable-smb`; mounts in the field are configured with it.
const SMB_DISABLE_ENV: &str = "SYNOLOGY_FS_SMB_DISABLE";
/// Predates the chain, and governed the SMB connect. It governs the probe too,
/// so one knob still means one thing.
const SMB_TIMEOUT_ENV: &str = "SYNOLOGY_FS_SMB_TIMEOUT_MS";

/// Whether SMB may be tried at all.
///
/// The environment variable has to reach the *policy*, not just the SMB
/// connect: the chain probes first, so a mount that set it would otherwise pay
/// the connect timeout it was trying to avoid and then report a transport it
/// is not using.
fn smb_disabled(flag: bool, env: Option<std::ffi::OsString>) -> bool {
    flag || env.is_some()
}

/// How long the probe waits for port 445.
fn probe_timeout(env: Option<String>) -> Duration {
    env.and_then(|s| s.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(2))
}

/// Parse a umask the way `umask(1)` and every mount helper do: octal, with no
/// `0o` prefix required. Base 10 would silently turn the near-universal `022`
/// into 0o026.
fn parse_umask(raw: &str) -> Result<u16, String> {
    let value = u16::from_str_radix(raw.trim_start_matches("0o"), 8)
        .map_err(|_| format!("`{raw}` is not an octal umask"))?;
    if value > 0o777 {
        return Err(format!("umask `{raw}` is out of range (max 777)"));
    }
    Ok(value)
}

fn prompt(label: &str) -> anyhow::Result<String> {
    eprint!("{}: ", label);
    io::stderr().flush()?;
    let mut value = String::new();
    io::stdin().read_line(&mut value)?;
    Ok(value.trim().to_string())
}

/// Whether the password was typed on the command line rather than coming from
/// the environment. argv is readable by any local account; `SYNO_PASSWORD` is
/// not, so only the former deserves a warning.
fn password_came_from_argv() -> bool {
    std::env::args().any(|a| a == "-p" || a == "--password" || a.starts_with("--password="))
}

/// Resolve the password from stdin, argv/environment, or an interactive prompt.
fn resolve_password(args: &Args) -> anyhow::Result<String> {
    if args.password_stdin {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_string());
    }
    match &args.password {
        Some(p) => {
            if password_came_from_argv() {
                tracing::warn!(
                    "--password puts the password in this process's argv, where every \
                     other account on this machine can read it with `ps`. Prefer \
                     SYNO_PASSWORD, --password-stdin, or the interactive prompt."
                );
            }
            Ok(p.clone())
        }
        None => Ok(rpassword::prompt_password("Password: ")?),
    }
}
/// Target for the single record naming the error that ended the run.
///
/// It exists to be filtered: the record belongs in the log file, which is read
/// after the fact and otherwise stops just short of saying why the run ended,
/// but not on the console, where the runtime is already about to print the same
/// error to stderr.
const FATAL_TARGET: &str = "synology_filestation_fuse::fatal";

/// Open the `--log-file` the arguments ask for, if any.
///
/// Split out of [`init_logging`] because this is the part with a decision in
/// it: only this call site knows the mountpoint, and so only this call site can
/// refuse a log path that would be written through the mount it describes.
fn log_file(args: &Args) -> anyhow::Result<Option<LogFile>> {
    let Some(requested) = args.log_file.as_deref() else {
        return Ok(None);
    };
    let path = logfile::resolve(requested, Some(&args.mountpoint))?;
    Ok(Some(LogFile::open(path)?))
}

/// Install the subscriber: stderr as always, plus the file when asked for.
///
/// Returns where the file went, so the mount can say so on the way up — a log
/// nobody can find is not much better than one that was never written.
fn init_logging(args: &Args) -> anyhow::Result<Option<PathBuf>> {
    use tracing_subscriber::layer::{Layer, SubscriberExt};
    use tracing_subscriber::util::SubscriberInitExt;

    // The console sees everything except the record naming the error that
    // ended the run: `main` already returns that error, and the runtime prints
    // it to stderr exactly as it always has. Letting it through here too would
    // print the one failure twice, on two different streams.
    let console = tracing_subscriber::fmt::layer().with_filter(
        tracing_subscriber::filter::filter_fn(|meta| meta.target() != FATAL_TARGET),
    );
    let registry = tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::new(&args.log_level))
        .with(console);

    match log_file(args)? {
        None => {
            registry.init();
            Ok(None)
        }
        Some(file) => {
            let path = file.path().to_path_buf();
            // No ANSI: this is read later, by `less` or by whoever is helping.
            let to_file = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(file);
            registry.with(to_file).init();
            Ok(Some(path))
        }
    }
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let log_path = init_logging(&args)?;
    if let Some(path) = &log_path {
        info!("Logging to {}", path.display());
    }

    // Record the failure before returning it. Handing it back to the runtime
    // prints it to stderr and nowhere else — so the log file, the one sink
    // written to be read after the fact, stopped just short of saying why the
    // run ended. It goes out under FATAL_TARGET, which the console layer drops,
    // so the error is still returned and the terminal shows it once, on stderr,
    // exactly as before.
    run(args).inspect_err(|e| tracing::error!(target: FATAL_TARGET, "{e:#}"))
}

fn run(args: Args) -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    info!(
        "Connecting to Synology NAS at {}://{}:{}",
        if args.https { "https" } else { "http" },
        args.host,
        args.port
    );

    let client = SynologyClient::new(&args.host, args.port, args.https);
    let client = if args.insecure {
        if args.https {
            tracing::warn!(
                "--insecure: TLS certificate verification is OFF; the connection \
                 is encrypted but not authenticated"
            );
        }
        client.with_insecure_tls()
    } else {
        client
    };

    let password = resolve_password(&args)?;

    let otp = args.otp.as_deref();
    let login_result = rt.block_on(client.login(&args.username, &password, otp));

    match login_result {
        Ok(()) => {}
        Err(ref e) if is_otp_required(e) => {
            let code = prompt("Two-factor authentication code")?;
            rt.block_on(client.login(&args.username, &password, Some(&code)))?;
        }
        // A TLS failure here is almost always a self-signed NAS certificate, and
        // the fix is a flag the user has no reason to know exists. Name it.
        Err(e) if args.https && !args.insecure && e.is_tls_error() => {
            return Err(anyhow::anyhow!(
                "could not verify the TLS certificate for {}:{} ({e}).\n\
                 \n\
                 If this NAS uses a self-signed certificate, either install it in \
                 your system trust store, or re-run with --insecure to accept any \
                 certificate (encrypted, but not authenticated).",
                args.host,
                args.port
            ));
        }
        Err(e) => return Err(e.into()),
    }

    info!("Logged in successfully");

    // Which way to reach the NAS: SMB, SMB through a tunnel, or the HTTP API.
    // The chain decides which leg reaches the NAS, and is asked again while
    // the mount runs; `--disable-*` says which legs it may consider at all.
    let policy = TransportPolicy::from_flags(
        smb_disabled(args.disable_smb, std::env::var_os(SMB_DISABLE_ENV)),
        args.disable_vpn,
        args.disable_https,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let endpoints = match &args.vpn_host {
        Some(inside) => Endpoints::with_tunnel(&args.host, inside),
        None => Endpoints::public_only(&args.host),
    };
    // Shared rather than owned: the SMB transport keeps a handle to it so a
    // dead tunnel can be asked for a new connection long after the mount
    // started, and the leg watch below asks it for better ones.
    let chain = Arc::new(Chain::new(
        policy,
        endpoints,
        Box::new(TcpProber::new(probe_timeout(
            std::env::var(SMB_TIMEOUT_ENV).ok(),
        ))),
        tunnel_from(&args, &args.username, &password),
        DEFAULT_RECHECK,
    ));

    // The profile is fetched before anything asks for a tunnel, over the
    // session just authenticated — which is what lets somebody outside the
    // NAS's network get the file that gets them inside it. Only when told
    // where it lives on the NAS; otherwise whatever is on disk is what there
    // is.
    if let (Some(local), Some(remote)) = (&args.vpn_profile, &args.vpn_profile_nas) {
        let source = ProfileSource {
            remote: remote.clone(),
            local: local.clone(),
        };
        if let Err(e) = rt.block_on(source.ensure(&client)) {
            warn!("VPN profile: {e}; the tunnel leg will not be available");
        }
    }

    // SMB goes on the best leg that answers now, and the mount keeps looking
    // for a better one for as long as it is up: one that came up on HTTP moves
    // to SMB when SMB becomes reachable, and one that came up through the
    // tunnel moves to a direct connection when that answers. The transport is
    // attached whether or not a leg answered — without a session it stands
    // aside for HTTP — so there is somewhere for the session to go.
    let smb_cfg = SmbConfig::for_account(
        &args.host,
        &args.username,
        &password,
        args.domain.as_deref(),
    );
    let legs = rt.block_on(Legs::start(chain.clone(), &smb_cfg));
    let on = legs
        .as_ref()
        .map_or(Transport::Https, |legs| legs.current());
    if on == Transport::Https && !policy.allows_https() {
        return Err(anyhow::anyhow!("{NoTransport}"));
    }
    let client = Arc::new(match &legs {
        Some(legs) => synology_filestation_smb::attach(client, legs.session().clone()),
        None => client,
    });
    // Aborted when it is dropped at the end of `main`, after the unmount.
    let _watching = legs
        .as_ref()
        .map(|legs| AbortOnDrop(legs.watch(rt.handle(), DEFAULT_WATCH_INTERVAL)));

    let opts = MountOptions {
        cache_ttl: args.cache_ttl,
        read_cache_mb: args.read_cache_mb,
        io_threads: args.fuse_threads,
        prefetch_blocks: args.prefetch_blocks,
        uid: args.uid,
        gid: args.gid,
        umask: args.umask,
    };
    let handle = spawn_mount(client.clone(), rt.handle().clone(), args.mountpoint, opts)?;

    // Block until Ctrl-C, then unmount and log out — preserving the previous
    // foreground CLI behaviour now that the mount itself runs in the background.
    rt.block_on(tokio::signal::ctrl_c())?;
    info!("Signal received, unmounting…");
    handle.stop();

    info!("Logging out");
    rt.block_on(client.logout())?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{parse_umask, probe_timeout, smb_disabled, tunnel_from, Args};
    use clap::Parser;
    use std::time::Duration;

    /// The arguments a mount is given, with the profile flag set or not.
    fn args_with(profile: Option<&str>) -> Args {
        let mut argv = vec![
            "synology-fuse",
            "--host",
            "nas.example",
            "--username",
            "someone",
            // Positional, and required.
            "/mnt/nas",
        ];
        if let Some(profile) = profile {
            argv.extend(["--vpn-profile", profile]);
        }
        Args::try_parse_from(argv).expect("the arguments parse")
    }

    #[tokio::test]
    async fn without_a_profile_there_is_no_tunnel_to_wait_for() {
        // A tunnel needs a profile: without one there is nothing to dial,
        // nothing to authenticate against, and no address to be given. Saying
        // so lets the chain fall from SMB straight to HTTP with nothing to
        // wait for, rather than timing out on an escalation that was never
        // possible.
        let tunnel = tunnel_from(&args_with(None), "someone", "hunter2");

        let Err(refused) = tunnel.open("10.90.24.1", 445).await else {
            panic!("there is no tunnel to open");
        };
        assert!(
            refused.to_string().contains("no tunnel is configured"),
            "and it says why: {refused}"
        );
    }

    #[tokio::test]
    async fn a_profile_makes_the_escalation_possible() {
        // The other half: given a profile, what the chain gets is something
        // that will actually try — here it fails on the file, which is the
        // real tunnel talking rather than the placeholder.
        let tunnel = tunnel_from(
            &args_with(Some("/nowhere/e4e-nas-vpn.ovpn")),
            "someone",
            "hunter2",
        );

        let Err(refused) = tunnel.open("10.90.24.1", 445).await else {
            panic!("that profile is not there");
        };
        assert!(
            refused.to_string().contains("e4e-nas-vpn.ovpn"),
            "it got as far as looking for the profile: {refused}"
        );
    }

    /// `--umask 022` must mean 0o022. Parsed as decimal it would be 0o026,
    /// quietly stripping group/other permissions the user never asked to drop.
    #[test]
    fn umask_is_parsed_as_octal() {
        assert_eq!(parse_umask("022"), Ok(0o022));
        assert_eq!(parse_umask("077"), Ok(0o077));
        assert_eq!(parse_umask("0o027"), Ok(0o027));
        assert_eq!(parse_umask("0"), Ok(0));
    }

    #[test]
    fn a_umask_outside_the_permission_bits_is_rejected() {
        assert!(parse_umask("1777").is_err());
        assert!(parse_umask("088").is_err(), "8 is not an octal digit");
        assert!(parse_umask("rwx").is_err());
    }

    /// The three `--disable-*` flags map onto the chain's policy, including
    /// the implication that matters: without SMB there is nothing for a tunnel
    /// to reach, so it is not dialled either.
    #[test]
    fn the_disable_flags_describe_which_legs_may_be_tried() {
        use synology_filestation_connect::TransportPolicy;

        let all = TransportPolicy::from_flags(false, false, false).unwrap();
        assert!(all.allows_smb() && all.allows_vpn() && all.allows_https());

        let no_smb = TransportPolicy::from_flags(true, false, false).unwrap();
        assert!(!no_smb.allows_smb());
        assert!(!no_smb.allows_vpn(), "a tunnel exists to reach SMB");
        assert!(no_smb.allows_https());

        let no_vpn = TransportPolicy::from_flags(false, true, false).unwrap();
        assert!(no_vpn.allows_smb(), "a reachable NAS is unaffected");
        assert!(!no_vpn.allows_vpn());

        // Nothing left to carry the data: refused at startup rather than
        // mounting something that cannot answer a read.
        assert!(TransportPolicy::from_flags(true, false, true).is_err());
    }

    /// The environment variable that predates `--disable-smb` has to reach the
    /// policy, not just the SMB connect. The chain probes first, so a mount
    /// that set it would otherwise pay the connect timeout it was avoiding —
    /// and then log a transport it is not using.
    ///
    /// Pure, so the suite never mutates the process environment: that is
    /// global state shared with every other test.
    #[test]
    fn the_old_disable_variable_still_turns_smb_off() {
        use std::ffi::OsString;

        assert!(!smb_disabled(false, None));
        assert!(smb_disabled(true, None), "the flag alone");
        assert!(
            smb_disabled(false, Some(OsString::from("1"))),
            "the variable alone"
        );
        // Its value never mattered; being set is the signal.
        assert!(smb_disabled(false, Some(OsString::from(""))));
    }

    #[test]
    fn the_probe_waits_as_long_as_the_smb_connect_would() {
        assert_eq!(probe_timeout(None), Duration::from_secs(2));
        assert_eq!(
            probe_timeout(Some("500".into())),
            Duration::from_millis(500)
        );
        // Nonsense falls back rather than failing the mount over a typo.
        assert_eq!(probe_timeout(Some("soon".into())), Duration::from_secs(2));
    }
}

#[cfg(test)]
mod log_file_tests {
    use super::*;

    fn args_with(extra: &[&str]) -> Args {
        let mut argv = vec!["synology-fuse", "--host", "nas.example", "-u", "someone"];
        argv.extend_from_slice(extra);
        Args::parse_from(argv)
    }

    /// The check belongs here, not only in `logfile`: it is this call site that
    /// knows the mountpoint. A log written through the mount it describes
    /// deadlocks the mount, and finding that out at the first `warn!` — inside
    /// a FUSE callback, with the machine already in trouble — is too late.
    #[test]
    fn a_log_file_inside_the_mountpoint_is_refused_before_the_mount_starts() {
        let dir = tempfile::tempdir().unwrap();
        let mountpoint = dir.path().join("mnt");
        std::fs::create_dir(&mountpoint).unwrap();
        let inside = mountpoint.join("mount.log");

        let args = args_with(&[
            "--log-file",
            inside.to_str().unwrap(),
            mountpoint.to_str().unwrap(),
        ]);

        let err = log_file(&args).expect_err("a log inside the mount must not be opened");
        assert!(
            err.to_string().contains("deadlock"),
            "the message has to say why, got: {err}"
        );
    }

    /// `--log-file` is opt-in, so the common invocation must be untouched: no
    /// file created, nothing to clean up, stderr exactly as before.
    #[test]
    fn without_the_flag_no_file_is_opened() {
        let args = args_with(&["/mnt/nas"]);

        assert!(log_file(&args).unwrap().is_none());
    }

    /// The file exists the moment the mount starts, not at the first record —
    /// a run that produced no output should still leave evidence it ran.
    #[test]
    fn the_requested_file_is_opened_up_front() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/mount.log");

        let args = args_with(&["--log-file", path.to_str().unwrap(), "/mnt/nas"]);

        let opened = log_file(&args).unwrap().expect("a file was asked for");
        assert_eq!(opened.path(), path);
        assert!(path.is_file(), "{} was not created", path.display());
    }
}
