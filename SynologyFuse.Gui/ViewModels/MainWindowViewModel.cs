using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Threading;
using System.Threading.Tasks;
using Avalonia.Threading;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using SynologyFuse.Gui.Interop;
using SynologyFuse.Gui.Models;
using SynologyFuse.Gui.Services;

namespace SynologyFuse.Gui.ViewModels;

public sealed partial class MainWindowViewModel : ObservableObject, IDisposable
{
    private readonly MountService _mountService = new();

    /// <summary>The log pane, bounded so a chatty session cannot grow it
    /// without limit. See <see cref="LogBuffer"/>.</summary>
    private readonly LogBuffer _log = new();

    /// <summary>1 while a pane refresh is already queued on the dispatcher, so
    /// a burst of lines coalesces into one update. `Interlocked`, because
    /// lines arrive off the UI thread.</summary>
    private int _logFlushPending;

    /// <summary>The action the OTP banner is gating: either a full mount or a
    /// connection test. Set when a connect attempt reports OTP-required so
    /// <see cref="SubmitOtp"/> knows which operation to retry.</summary>
    private enum PendingAction { None, Mount, Test }

    private PendingAction _pending = PendingAction.None;
    private MountConfig? _pendingConfig;

    public MainWindowViewModel()
    {
        var s = SettingsService.Load();
        _host = s.Host;
        _username = s.Username;
        _port = s.Port;
        _useHttps = s.UseHttps;
        _verifySsl = s.VerifySsl;
        _mountpoint = s.Mountpoint;
        _cacheTtl = s.CacheTtl;
        _readCacheMb = s.ReadCacheMb;
        _prefetchBlocks = s.PrefetchBlocks;
        _logLevel = s.LogLevel;
        _logToFile = s.LogToFile;
        _logFilePath = s.LogFilePath;
        _domain = s.Domain;
        _vpnProfile = s.VpnProfile;
        _vpnHost = s.VpnHost;
        _vpnProfileNas = s.VpnProfileNas;

        _mountService.OutputReceived += OnOutput;

        var v = UpdateCheckService.CurrentVersion();
        Version = $"v{v.Major}.{v.Minor}.{v.Build}";

        _ = CheckForUpdatesAsync();
    }

    public string Version { get; }

    private async Task CheckForUpdatesAsync()
    {
        var info = await UpdateCheckService.CheckAsync();
        if (info is null) return;

        Dispatcher.UIThread.Post(() =>
        {
            UpdateVersion = info.Latest.ToString();
            UpdateUrl = info.HtmlUrl;
            ShowUpdateBanner = true;
        });
    }

    // ── Connection fields ─────────────────────────────────────────────────────

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(ConnectCommand))]
    [NotifyCanExecuteChangedFor(nameof(TestConnectionCommand))]
    private string _host;

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(ConnectCommand))]
    [NotifyCanExecuteChangedFor(nameof(TestConnectionCommand))]
    private string _username;

    [ObservableProperty]
    private string _password = "";

    [ObservableProperty]
    private decimal _port;

    [ObservableProperty]
    private bool _useHttps;

    /// <summary>Verify the NAS TLS certificate. Unticking this accepts any
    /// certificate: encrypted, but not authenticated.</summary>
    [ObservableProperty]
    private bool _verifySsl = true;

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(ConnectCommand))]
    private string _mountpoint;

    // ── Advanced ──────────────────────────────────────────────────────────────

    [ObservableProperty]
    private decimal _cacheTtl;

    [ObservableProperty]
    private decimal _readCacheMb;

    /// <summary>Speculative read-ahead depth in 256 KiB blocks; 0 switches it
    /// off. Worth turning down for a bulk walk over a corpus, which reads each
    /// file once and gets nothing back for the blocks fetched ahead.</summary>
    [ObservableProperty]
    private decimal _prefetchBlocks;

    [ObservableProperty]
    private string _logLevel = "info"; // overwritten by constructor; default satisfies nullable analysis

    /// <summary>NetBIOS domain the account lives in — `KRG` for an AD account,
    /// empty for a local DSM user. Both directory-authenticated legs need it:
    /// empty against an AD account is why a connection ends up on the slower
    /// HTTP path without saying so, and why the VPN is refused before the
    /// password is checked.</summary>
    [ObservableProperty]
    private string _domain = "";

    /// <summary>The OpenVPN profile on this computer, if there is already one
    /// here. Left empty, one is downloaded to a copy kept beside the settings —
    /// provided <see cref="VpnProfileNas"/> says where to find it.</summary>
    [ObservableProperty]
    private string _vpnProfile = "";

    /// <summary>The same profile's path on the NAS, to download it from.
    ///
    /// Separate from <see cref="VpnProfile"/> because they are different
    /// places: one field asking for "the VPN profile" invites the NAS's path
    /// into a setting that names a file on this disk, and the connection then
    /// fails trying to create that directory here.</summary>
    [ObservableProperty]
    private string _vpnProfileNas = "";

    /// <summary>The NAS's address inside that tunnel, which its public name
    /// does not resolve to.</summary>
    [ObservableProperty]
    private string _vpnHost = "";

    public IReadOnlyList<string> LogLevels { get; } =
        ["error", "warn", "info", "debug", "trace"];

    /// <summary>Also write the log to a file, so it outlives the window.
    ///
    /// The log pane is gone the moment the process is — including when a wedged
    /// mount takes the machine down with it, which is exactly the run somebody
    /// will want to read afterwards.</summary>
    [ObservableProperty]
    private bool _logToFile;

    /// <summary>Where that file goes. Empty means
    /// <see cref="SettingsService.DefaultLogFilePath"/>, so ticking the box is
    /// enough to get a log.</summary>
    [ObservableProperty]
    private string _logFilePath = "";

    /// <summary>Shown beside the empty path box, so somebody who never types
    /// one still knows where to look afterwards.</summary>
    public string DefaultLogFilePath => SettingsService.DefaultLogFilePath;

    // ── State ─────────────────────────────────────────────────────────────────

    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsDisconnected))]
    [NotifyCanExecuteChangedFor(nameof(ConnectCommand))]
    [NotifyCanExecuteChangedFor(nameof(DisconnectCommand))]
    [NotifyCanExecuteChangedFor(nameof(TestConnectionCommand))]
    private bool _isConnected;

    public bool IsDisconnected => !IsConnected;

    /// <summary>True while a connect / test / mount attempt is in flight. Drives
    /// the spinner and disables the action buttons.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(IsIdle))]
    [NotifyCanExecuteChangedFor(nameof(ConnectCommand))]
    [NotifyCanExecuteChangedFor(nameof(TestConnectionCommand))]
    [NotifyCanExecuteChangedFor(nameof(DisconnectCommand))]
    private bool _isConnecting;

    public bool IsIdle => !IsConnecting;

    [ObservableProperty]
    private string _statusText = "Disconnected";

    /// <summary>Which leg the connection reached the NAS by.
    ///
    /// Shown as a badge, because the difference between these is the difference
    /// between a transfer that resumes where it stopped and one that starts
    /// again — and until now nothing told anyone which they had.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(TransportBadge))]
    [NotifyPropertyChangedFor(nameof(TransportDetail))]
    [NotifyPropertyChangedFor(nameof(HasTransport))]
    private SynoTransport _transport = SynoTransport.Unknown;

    public bool HasTransport => Transport != SynoTransport.Unknown;

    public string TransportBadge => TransportPresenter.Badge(Transport);

    public string TransportDetail => TransportPresenter.Detail(Transport);

    [ObservableProperty]
    private string _logOutput = "";

    /// <summary>True when a 2FA code is required to finish connecting. Shows an
    /// inline banner so the user can supply the code and retry.</summary>
    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(SubmitOtpCommand))]
    private bool _showOtpPrompt;

    /// <summary>The code the user typed in the 2FA banner.</summary>
    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(SubmitOtpCommand))]
    private string _pendingOtp = "";

    // ── Error banner ──────────────────────────────────────────────────────────

    /// <summary>What went wrong and what to do about it. Filled in by any
    /// connect / mount / disconnect failure and cleared when the next attempt
    /// starts, so a stale failure never sits next to a fresh success.</summary>
    public ErrorBannerViewModel ErrorBanner { get; } = new();

    // ── Update banner ─────────────────────────────────────────────────────────

    [ObservableProperty]
    private bool _showUpdateBanner;

    [ObservableProperty]
    private string _updateVersion = "";

    [ObservableProperty]
    private string _updateUrl = "";

    [RelayCommand]
    private void DismissUpdateBanner() => ShowUpdateBanner = false;

    [RelayCommand]
    private void OpenUpdateUrl()
    {
        if (string.IsNullOrEmpty(UpdateUrl)) return;
        try
        {
            Process.Start(new ProcessStartInfo
            {
                FileName = UpdateUrl,
                UseShellExecute = true,
            });
        }
        catch (Exception ex)
        {
            AppendLog($"Could not open browser: {ex.Message}");
        }
    }

    // ── Commands ──────────────────────────────────────────────────────────────

    [RelayCommand(CanExecute = nameof(CanConnect))]
    private async Task Connect()
    {
        PersistSettings();
        _pendingConfig = BuildConfig();
        await RunConnectAsync(PendingAction.Mount, _pendingConfig, otp: null);
    }

    [RelayCommand(CanExecute = nameof(CanConnect))]
    private async Task TestConnection()
    {
        PersistSettings();
        _pendingConfig = BuildConfig();
        await RunConnectAsync(PendingAction.Test, _pendingConfig, otp: null);
    }

    /// <summary>Shared connect path for both Mount and Test. Handles the spinner,
    /// the OTP-required banner, and error reporting uniformly.</summary>
    private async Task RunConnectAsync(PendingAction action, MountConfig config, string? otp)
    {
        ShowOtpPrompt = false;
        ErrorBanner.Clear();
        IsConnecting = true;
        StatusText = action == PendingAction.Test ? "Testing connection…" : "Connecting…";
        AppendLog(action == PendingAction.Test
            ? $"Testing connection to {config.Host}…"
            : $"Connecting to {config.Host}…");

        try
        {
            if (action == PendingAction.Test)
            {
                await _mountService.TestConnectionAsync(config, otp);
                Transport = _mountService.Transport;
                StatusText = "Connection OK";
                AppendLog($"Connection succeeded, using {TransportBadge}.");
                _pending = PendingAction.None;
            }
            else
            {
                await _mountService.ConnectAndMountAsync(config, otp);
                Transport = _mountService.Transport;
                IsConnected = true;
                StatusText = $"Volume ready at {config.Mountpoint}";
                AppendLog($"Volume ready, using {TransportBadge}.");
                _pending = PendingAction.None;
            }
        }
        catch (OtpRequiredException)
        {
            // Not an error: stash the in-flight action and prompt for the code.
            _pending = action;
            ShowOtpPrompt = true;
            StatusText = "2FA code required";
            AppendLog("[2FA] Two-factor authentication code required.");
        }
        catch (Exception ex)
        {
            IsConnected = false;
            Report(ex);
        }
        finally
        {
            IsConnecting = false;
        }
    }

    private bool CanConnect() =>
        !IsConnected &&
        !IsConnecting &&
        !string.IsNullOrWhiteSpace(Host) &&
        !string.IsNullOrWhiteSpace(Username) &&
        !string.IsNullOrWhiteSpace(Mountpoint);

    [RelayCommand(CanExecute = nameof(CanDisconnect))]
    private async Task Disconnect()
    {
        AppendLog("Disconnecting…");
        StatusText = "Disconnecting…";
        ShowOtpPrompt = false;
        ErrorBanner.Clear();
        IsConnecting = true;
        try
        {
            // Stop() disposes the native client and blocks while unmounting and
            // joining the background worker — keep it off the UI thread.
            await _mountService.StopAsync(onSlow: () => Dispatcher.UIThread.Post(() =>
            {
                // Said, not done. The volume is already unmounted — that is the
                // first thing teardown does — and the session is still closing.
                // The spinner stays because it is still true, and Connect stays
                // disabled because this is not finished.
                StatusText = "Still closing the session — the volume is unmounted";
                AppendLog(
                    "Closing the session is taking more than "
                    + $"{MountService.SlowStopThreshold.TotalSeconds:0} seconds. The volume is "
                    + "already unmounted; the last 'teardown:' line above is the step it is on.");
            }));
            IsConnected = false;
            // Nothing is being reached any more, so the badge goes with it: a
            // "SMB" next to "Disconnected" is a guess about a connection that
            // no longer exists, and the next one may not get the same leg.
            Transport = SynoTransport.Unknown;
            StatusText = "Disconnected";
            AppendLog("Volume closed.");
        }
        catch (Exception ex)
        {
            Report(ex);
        }
        finally
        {
            IsConnecting = false;
        }
    }

    private bool CanDisconnect() => IsConnected && !IsConnecting;

    /// <summary>Retry the gated connect/test with the user-entered OTP code.</summary>
    [RelayCommand(CanExecute = nameof(CanSubmitOtp))]
    private async Task SubmitOtp()
    {
        var code = PendingOtp.Trim();
        var action = _pending;
        var config = _pendingConfig;
        ShowOtpPrompt = false;
        PendingOtp = "";
        AppendLog("Submitting 2FA code…");

        if (config is null || action == PendingAction.None) return;
        await RunConnectAsync(action, config, code);
    }

    private bool CanSubmitOtp() =>
        ShowOtpPrompt && !string.IsNullOrWhiteSpace(PendingOtp);

    // ── Helpers ─────────────────────────────────────────────────────────────────

    /// <summary>Snapshot of the current form as a <see cref="MountConfig"/>,
    /// used by the file browser window.</summary>
    public MountConfig SnapshotConfig() => BuildConfig();

    private MountConfig BuildConfig() => new()
    {
        Host = Host,
        Username = Username,
        Password = Password,
        Port = (ushort)Port,
        UseHttps = UseHttps,
        VerifySsl = VerifySsl,
        Mountpoint = Mountpoint,
        CacheTtl = (ulong)CacheTtl,
        ReadCacheMb = (ulong)ReadCacheMb,
        PrefetchBlocks = (ulong)PrefetchBlocks,
        LogLevel = LogLevel,
        LogFile = SettingsService.ResolveLogFile(LogToFile, LogFilePath),
        Domain = Domain,
        VpnProfile = VpnProfile,
        VpnHost = VpnHost,
        VpnProfileNas = VpnProfileNas,
    };

    private void PersistSettings() => SettingsService.Save(new PersistedSettings
    {
        Host = Host,
        Username = Username,
        Port = Port,
        UseHttps = UseHttps,
        VerifySsl = VerifySsl,
        Mountpoint = Mountpoint,
        CacheTtl = CacheTtl,
        ReadCacheMb = ReadCacheMb,
        PrefetchBlocks = PrefetchBlocks,
        LogLevel = LogLevel,
        LogToFile = LogToFile,
        LogFilePath = LogFilePath,
        Domain = Domain,
        VpnProfile = VpnProfile,
        VpnHost = VpnHost,
        VpnProfileNas = VpnProfileNas,
    });

    // ── Event handlers ────────────────────────────────────────────────────────

    private void OnOutput(string line) => AppendLog(line);

    /// <summary>Surface a failure everywhere it belongs: the banner carries the
    /// cause and the remedy, the status bar the headline, the log pane the raw
    /// message. Previously a failure was only ever the word "Error" plus a log
    /// line, which never said what to fix.</summary>
    private void Report(Exception ex)
    {
        ErrorBanner.Show(ex);
        StatusText = ErrorBanner.Title;
        AppendLog(ErrorBanner.HasDetail
            ? $"Error: {ErrorBanner.Title} — {ErrorBanner.Detail}"
            : $"Error: {ErrorBanner.Title}");
    }

    /// <summary>
    /// Record a line and arrange for the pane to catch up.
    ///
    /// The line goes into the buffer on whatever thread produced it — the
    /// native log callback does not run on the UI thread — and the pane is
    /// refreshed once per dispatcher turn rather than once per line. That
    /// coalescing is the point: a VPN handshake at debug level delivers lines
    /// far faster than a TextBox can be re-rendered, and posting one update
    /// each is what stopped the window responding.
    /// </summary>
    private void AppendLog(string line)
    {
        _log.Append(line);

        // Already queued: the flush that runs will pick this line up too, so
        // a burst of a thousand lines costs one update rather than a thousand.
        if (Interlocked.Exchange(ref _logFlushPending, 1) == 1) return;

        Dispatcher.UIThread.Post(
            () =>
            {
                Interlocked.Exchange(ref _logFlushPending, 0);
                LogOutput = _log.Text;
            },
            // Behind anything the user is waiting on. The log is context, not
            // the task.
            DispatcherPriority.Background);
    }

    public void Dispose()
    {
        _mountService.OutputReceived -= OnOutput;
        _mountService.Dispose();
    }
}
