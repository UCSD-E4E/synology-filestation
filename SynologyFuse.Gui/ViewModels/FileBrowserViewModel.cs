using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.IO;
using System.Threading;
using System.Threading.Tasks;
using Avalonia.Threading;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using SynologyFuse.Gui.Interop;
using SynologyFuse.Gui.Models;
using SynologyFuse.Gui.Services;

namespace SynologyFuse.Gui.ViewModels;

/// <summary>
/// Backs the pre-mount file browser. Owns its own <see cref="SynoClient"/>
/// session (connect on open, dispose on close) and exercises the full client
/// surface: list shares/dirs, download, upload, delete, create folder. Browse
/// works without mounting anything.
/// </summary>
public sealed partial class FileBrowserViewModel : ObservableObject, IDisposable
{
    private readonly MountConfig _config;
    private readonly IClipboardService? _clipboard;
    private SynoClient? _client;
    private bool _disposed;

    // Serializes every native call against Dispose() so the handle is never
    // freed while a P/Invoke is in flight. The gate is taken and released on
    // the worker thread inside Task.Run (never the UI thread), so Dispose() can
    // block on it without risking a deadlock.
    private readonly SemaphoreSlim _gate = new(1, 1);

    /// <summary>Path stack for breadcrumb navigation; "" denotes the shares root.</summary>
    private readonly Stack<string> _history = new();

    /// <summary>Asks the NAS for a file's MD5; null when the client went away
    /// mid-call. Uses the native call unless a test supplies its own.</summary>
    private readonly Func<string, Task<string?>> _md5;

    public FileBrowserViewModel(MountConfig config, IClipboardService? clipboard = null)
        : this(config, clipboard, null)
    {
    }

    /// <summary>Seam for tests: <paramref name="md5"/> stands in for the native
    /// hash call, which otherwise needs a connected session.</summary>
    internal FileBrowserViewModel(
        MountConfig config, IClipboardService? clipboard, Func<string, Task<string?>>? md5)
    {
        _config = config;
        _clipboard = clipboard;
        _md5 = md5 ?? (path => GatedAsync(c => c.Md5(path)));
    }

    public ObservableCollection<SynoFileInfo> Items { get; } = new();

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(UpCommand))]
    [NotifyCanExecuteChangedFor(nameof(RefreshCommand))]
    [NotifyCanExecuteChangedFor(nameof(CopyCurrentPathCommand))]
    [NotifyPropertyChangedFor(nameof(Title))]
    private string _currentPath = "";

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(CopySelectedPathCommand))]
    [NotifyCanExecuteChangedFor(nameof(CopySelectedNameCommand))]
    [NotifyCanExecuteChangedFor(nameof(ComputeMd5Command))]
    private SynoFileInfo? _selectedItem;

    /// <summary>Window title — carries the directory being browsed so the path
    /// is visible (and copyable, via <see cref="CopyCurrentPathCommand"/>) at a glance.</summary>
    public string Title => CurrentPath.Length == 0 ? "Browse NAS" : $"Browse NAS — {CurrentPath}";

    [ObservableProperty]
    private string _status = "Connecting…";

    /// <summary>Anything this window is waiting on the NAS for. A hash may not
    /// start under it: it would take over the status and progress of the
    /// work already running, and queue behind it on the gate regardless.</summary>
    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(ComputeMd5Command))]
    private bool _isBusy;

    // ── 2FA ─────────────────────────────────────────────────────────────────────

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(SubmitOtpCommand))]
    private bool _showOtpPrompt;

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(SubmitOtpCommand))]
    private string _pendingOtp = "";

    // ── Transfer progress ─────────────────────────────────────────────────────

    /// <summary>The one progress bar, which a transfer and a hash cannot share:
    /// a hash started mid-download turned the byte count into an endless sweep,
    /// and whichever finished first hid the bar on the other.</summary>
    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(ComputeMd5Command))]
    private bool _showProgress;

    [ObservableProperty]
    private double _progressValue;

    [ObservableProperty]
    private double _progressMax = 1;

    [ObservableProperty]
    private string _progressText = "";

    /// <summary>For work that reports no progress — DSM's MD5 task says only
    /// "not finished yet" — so the bar moves without claiming a fraction.</summary>
    [ObservableProperty]
    private bool _progressIndeterminate;

    // ── MD5 result ────────────────────────────────────────────────────────────

    /// <summary>The last digest the NAS produced, as lowercase hex; "" when none.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasMd5Result))]
    [NotifyCanExecuteChangedFor(nameof(CopyMd5Command))]
    private string _md5Result = "";

    /// <summary>Which file <see cref="Md5Result"/> belongs to — the selection may
    /// have moved on since.</summary>
    [ObservableProperty]
    private string _md5FileName = "";

    public bool HasMd5Result => Md5Result.Length != 0;

    [ObservableProperty]
    [NotifyCanExecuteChangedFor(nameof(ComputeMd5Command))]
    private bool _isHashing;

    /// <summary>The file being hashed, for the message that turns a transfer away.</summary>
    private string _hashingName = "";

    public bool IsConnected => _client is not null;

    // ── Connect ──────────────────────────────────────────────────────────────────

    /// <summary>Connect the browser's session. Call once when the window opens.</summary>
    public Task ConnectAsync() => ConnectAsync(null);

    private async Task ConnectAsync(string? otp)
    {
        IsBusy = true;
        Status = "Connecting…";
        try
        {
            // The same access settings the mount uses: this is its own session,
            // so without them the browser's downloads stay on the
            // non-resumable HTTP path even when the mount would not.
            var client = await Task.Run(() => SynoClient.Connect(
                _config.Host, _config.Port, _config.UseHttps, _config.Username, _config.Password, otp,
                autoRelogin: true, verifySsl: _config.VerifySsl,
                domain: MountService.Blank(_config.Domain),
                vpnProfile: MountService.VpnProfileFor(_config),
                vpnHost: MountService.Blank(_config.VpnHost),
                vpnProfileRemote: MountService.Blank(_config.VpnProfileNas)));
            // If the window was closed (Dispose ran) while the login was in
            // flight, the handle would otherwise leak — dispose it and bail.
            // The await resumed on the UI thread, so this can't race Dispose().
            if (_disposed)
            {
                client.Dispose();
                return;
            }
            _client = client;
            ShowOtpPrompt = false;
            OnPropertyChanged(nameof(IsConnected));
            await LoadAsync("");
        }
        catch (OtpRequiredException)
        {
            ShowOtpPrompt = true;
            Status = "2FA code required";
        }
        catch (Exception ex)
        {
            // Same classification the main window's error banner uses, so a
            // failed browse-window login reads like a sentence rather than a
            // native error string.
            Status = ErrorPresenter.Describe(ex).Title;
        }
        finally
        {
            IsBusy = false;
        }
    }

    [RelayCommand(CanExecute = nameof(CanSubmitOtp))]
    private async Task SubmitOtp()
    {
        var code = PendingOtp.Trim();
        ShowOtpPrompt = false;
        PendingOtp = "";
        await ConnectAsync(code);
    }

    private bool CanSubmitOtp() => ShowOtpPrompt && !string.IsNullOrWhiteSpace(PendingOtp);

    // ── Navigation ───────────────────────────────────────────────────────────────

    private async Task LoadAsync(string path)
    {
        if (RefusedWhileHashing() || _client is null) return;
        IsBusy = true;
        Status = path.Length == 0 ? "Loading shares…" : $"Loading {path}…";
        try
        {
            var entries = await GatedAsync(c => path.Length == 0 ? c.ListShares() : c.ListDir(path));
            if (entries is null) return; // client was disposed mid-call

            Items.Clear();
            foreach (var e in entries) Items.Add(e);
            CurrentPath = path;
            SelectedItem = null;
            Status = $"{Items.Count} item(s)" + (path.Length == 0 ? " (shares)" : $" in {path}");
        }
        catch (Exception ex)
        {
            Status = $"Error: {ex.Message}";
        }
        finally
        {
            IsBusy = false;
            RefreshCommand.NotifyCanExecuteChanged();
            UpCommand.NotifyCanExecuteChanged();
        }
    }

    /// <summary>Descend into the selected directory (no-op for files).</summary>
    [RelayCommand]
    private async Task Open()
    {
        if (RefusedWhileHashing() || SelectedItem is not { IsDir: true } dir) return;
        _history.Push(CurrentPath);
        await LoadAsync(dir.Path);
    }

    [RelayCommand(CanExecute = nameof(CanGoUp))]
    private async Task Up()
    {
        // Before the pop: a refused step up must not lose its way back.
        if (RefusedWhileHashing()) return;
        var target = _history.Count > 0 ? _history.Pop() : "";
        await LoadAsync(target);
    }

    private bool CanGoUp() => CurrentPath.Length != 0;

    [RelayCommand(CanExecute = nameof(CanRefresh))]
    private Task Refresh() => LoadAsync(CurrentPath);

    private bool CanRefresh() => IsConnected;

    // ── Clipboard ────────────────────────────────────────────────────────────────

    /// <summary>Copy the directory currently being browsed (the path bar / title).</summary>
    [RelayCommand(CanExecute = nameof(CanCopyCurrentPath))]
    private Task CopyCurrentPath() => CopyAsync(CurrentPath);

    private bool CanCopyCurrentPath() => CurrentPath.Length != 0;

    /// <summary>Copy the full NAS path of the selected file or folder.</summary>
    [RelayCommand(CanExecute = nameof(HasSelection))]
    private Task CopySelectedPath() => CopyAsync(SelectedItem?.Path ?? "");

    /// <summary>Copy just the name of the selected file or folder.</summary>
    [RelayCommand(CanExecute = nameof(HasSelection))]
    private Task CopySelectedName() => CopyAsync(SelectedItem?.Name ?? "");

    private bool HasSelection() => SelectedItem is not null;

    private async Task CopyAsync(string text)
    {
        if (text.Length == 0) return;
        try
        {
            // Null when no window is attached (headless/tests) — copying is then a no-op.
            if (_clipboard is not null) await _clipboard.SetTextAsync(text);
            Status = $"Copied {text}";
        }
        catch (Exception ex)
        {
            Status = $"Error: {ex.Message}";
        }
    }

    // ── Operations (invoked by the view, which supplies file-picker paths) ──────

    public async Task DownloadAsync(SynoFileInfo item, string localPath)
    {
        if (RefusedWhileHashing() || _client is null || item.IsDir) return;
        await RunWithProgressAsync($"Downloading {item.Name}…", client =>
            client.DownloadTo(item.Path, localPath, ReportProgress));
    }

    public async Task UploadAsync(string localPath)
    {
        if (RefusedWhileHashing() || _client is null || CurrentPath.Length == 0) return; // can't upload to the shares root
        var name = System.IO.Path.GetFileName(localPath);
        await RunWithProgressAsync($"Uploading {name}…", client =>
            client.Upload(localPath, CurrentPath, overwrite: true, ReportProgress));
        await LoadAsync(CurrentPath);
    }

    [RelayCommand]
    private async Task Delete()
    {
        if (RefusedWhileHashing() || _client is null || SelectedItem is null) return;
        var item = SelectedItem;
        await RunWithProgressAsync($"Deleting {item.Name}…", client => client.Delete(item.Path));
        await LoadAsync(CurrentPath);
    }

    public async Task CreateFolderAsync(string name)
    {
        if (RefusedWhileHashing() || _client is null || CurrentPath.Length == 0 || string.IsNullOrWhiteSpace(name)) return;
        await RunWithProgressAsync($"Creating {name}…", client => client.CreateFolder(CurrentPath, name));
        await LoadAsync(CurrentPath);
    }

    /// <summary>Have the NAS hash the selected file. DSM reads the whole file to
    /// answer — minutes for a large one — and reports nothing until it is done,
    /// so the bar is indeterminate. The digest lands in <see cref="Md5Result"/>,
    /// where it can be selected or copied.</summary>
    [RelayCommand(CanExecute = nameof(CanComputeMd5))]
    private async Task ComputeMd5()
    {
        if (SelectedItem is not { IsDir: false } item) return;
        Md5Result = "";
        Md5FileName = "";
        _hashingName = item.Name;
        IsHashing = true;
        IsBusy = true;
        ShowProgress = true;
        ProgressIndeterminate = true;
        ProgressText = $"The NAS is reading {item.Name}; a large file takes minutes.";
        Status = $"Computing MD5 of {item.Name}…";
        try
        {
            var digest = await _md5(item.Path);
            if (digest is null) return; // client was disposed mid-call
            Md5FileName = item.Name;
            Md5Result = digest;
            Status = $"MD5 of {item.Name} computed";
        }
        catch (Exception ex)
        {
            // The same classification the connect path and the main window's
            // banner use, so a refusal names the cause rather than a DSM number.
            Status = $"Could not compute the MD5 of {item.Name}: {ErrorPresenter.Describe(ex).Title}";
        }
        finally
        {
            ProgressIndeterminate = false;
            ShowProgress = false;
            IsBusy = false;
            IsHashing = false;
        }
    }

    private bool CanComputeMd5() => SelectedItem is { IsDir: false } && !IsHashing && !ShowProgress && !IsBusy;

    /// <summary>Turn away a transfer while a hash runs. It would only queue
    /// behind the hash on the gate — for minutes, with nothing on screen to
    /// say why — and take the progress bar from it.</summary>
    private bool RefusedWhileHashing()
    {
        if (!IsHashing) return false;
        Status = $"Wait for the MD5 of {_hashingName} to finish first: this window sends the NAS one request at a time.";
        return true;
    }

    [RelayCommand(CanExecute = nameof(HasMd5Result))]
    private Task CopyMd5() => CopyAsync(Md5Result);

    private async Task RunWithProgressAsync(string status, Action<SynoClient> op)
    {
        if (_client is null) return;
        IsBusy = true;
        ShowProgress = true;
        ProgressValue = 0;
        ProgressMax = 1;
        ProgressText = status;
        Status = status;
        try
        {
            await GatedAsync(op);
            Status = "Done";
        }
        catch (Exception ex)
        {
            Status = $"Error: {ex.Message}";
        }
        finally
        {
            ShowProgress = false;
            IsBusy = false;
        }
    }

    /// <summary>Run a native call that returns a value under the gate, on a
    /// worker thread. Returns null if the client was disposed.</summary>
    private Task<T?> GatedAsync<T>(Func<SynoClient, T> op) where T : class =>
        Task.Run<T?>(() =>
        {
            _gate.Wait();
            try
            {
                var c = _client;
                return c is null ? null : op(c);
            }
            finally { _gate.Release(); }
        });

    /// <summary>Run a native call with no return value under the gate.</summary>
    private Task GatedAsync(Action<SynoClient> op) =>
        Task.Run(() =>
        {
            _gate.Wait();
            try
            {
                var c = _client;
                if (c is not null) op(c);
            }
            finally { _gate.Release(); }
        });

    // Marshals native-thread progress callbacks onto the UI thread.
    private void ReportProgress(long done, long total) =>
        Dispatcher.UIThread.Post(() =>
        {
            ProgressMax = total > 0 ? total : Math.Max(done, 1);
            ProgressValue = done;
            ProgressText = total > 0
                ? $"{BrowserConverters.HumanSize(done)} / {BrowserConverters.HumanSize(total)}"
                : BrowserConverters.HumanSize(done);
        });

    public void Dispose()
    {
        _disposed = true;
        var client = _client;
        _client = null; // new GatedAsync calls now see null and bail
        if (client is null) return;

        // Free the handle off the UI thread: wait for any in-flight native op
        // (which holds the gate on a worker thread) to finish, THEN dispose.
        // Doing this on a background task keeps the window from freezing when
        // it's closed mid-transfer, while the gate still prevents a
        // dispose-during-P/Invoke use-after-free.
        Task.Run(() =>
        {
            _gate.Wait();
            try { client.Dispose(); }
            finally { _gate.Release(); }
        });
    }
}
