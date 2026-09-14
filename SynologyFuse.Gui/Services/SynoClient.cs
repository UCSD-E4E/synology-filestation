using System;
using System.Runtime.InteropServices;
using System.Text.Json;
using SynologyFuse.Gui.Interop;
using SynologyFuse.Gui.Models;
using static SynologyFuse.Gui.Interop.NativeMethods;

namespace SynologyFuse.Gui.Services;

/// <summary>
/// Managed wrapper over the native FileStation client. Owns the opaque native
/// client handle and (once mounted) the mount handle, translating native
/// status codes into <see cref="SynoException"/>s and JSON payloads into
/// <see cref="SynoFileInfo"/>s.
///
/// All calls block on the native Tokio runtime; callers should invoke browse /
/// transfer methods off the UI thread.
/// </summary>
public sealed class SynoClient : IDisposable
{
    private IntPtr _client;
    private IntPtr _mount;
    private bool _disposed;

    static SynoClient() => NativeMethods.EnsureResolver();

    private SynoClient(IntPtr client) => _client = client;

    /// <summary>True once <see cref="Mount"/> has succeeded and not been undone.</summary>
    public bool IsMounted => _mount != IntPtr.Zero;

    // ── Connect ────────────────────────────────────────────────────────────────

    /// <summary>
    /// Log in and return a connected client. Throws
    /// <see cref="OtpRequiredException"/> when the account needs a 2FA code (the
    /// caller should prompt and retry with <paramref name="otp"/> set), or
    /// <see cref="SynoException"/> on any other failure.
    /// </summary>
    /// <param name="verifySsl">Verify the NAS TLS certificate. Pass false only
    /// for a self-signed DSM certificate — the connection is then encrypted but
    /// not authenticated. Throws <see cref="TlsVerificationException"/> when
    /// verification is on and the certificate is rejected.</param>
    /// <param name="domain">NetBIOS domain the account lives in — `KRG` for an
    /// AD account, null for a local DSM user. Without it an AD account is
    /// checked against the appliance's own accounts, fails, and SMB is silently
    /// skipped in favour of the slower HTTP path; the VPN, meanwhile, refuses
    /// an unqualified name before it looks at the password.</param>
    /// <param name="vpnProfile">Where the OpenVPN profile is kept. Given it, a
    /// NAS that does not answer directly is reached through a tunnel raised
    /// inside this process — no tun device and no privileged helper. Fetched
    /// from the NAS if the file is not there.</param>
    /// <param name="vpnHost">The NAS's address inside that tunnel, which its
    /// public name does not resolve to.</param>
    public static SynoClient Connect(
        string host, ushort port, bool https, string username, string password,
        string? otp = null, bool autoRelogin = true, bool verifySsl = true,
        string? domain = null, string? vpnProfile = null, string? vpnHost = null,
        string? vpnProfileRemote = null)
    {
        NativeMethods.EnsureResolver();
        var err = default(NativeError);
        int rc = syno_connect(host, port, https, username, password, otp, autoRelogin,
            verifySsl, domain, vpnProfile, vpnHost, vpnProfileRemote, out var handle, ref err);
        Check(rc, ref err);
        return new SynoClient(handle);
    }

    /// <summary>Which leg this connection is using. Settled when it was made.</summary>
    public SynoTransport Transport => (SynoTransport)syno_transport(_client);

    // ── Browse ─────────────────────────────────────────────────────────────────

    public SynoFileInfo[] ListShares()
    {
        var err = default(NativeError);
        int rc = syno_list_shares(_client, out var json, ref err);
        Check(rc, ref err);
        return ParseArray(json);
    }

    public SynoFileInfo[] ListDir(string path)
    {
        var err = default(NativeError);
        int rc = syno_list_dir(_client, path, out var json, ref err);
        Check(rc, ref err);
        return ParseArray(json);
    }

    public SynoFileInfo GetInfo(string path)
    {
        var err = default(NativeError);
        int rc = syno_get_info(_client, path, out var json, ref err);
        Check(rc, ref err);
        return ParseObject(json);
    }

    // ── Transfers ──────────────────────────────────────────────────────────────

    /// <summary>Download <paramref name="remote"/> to <paramref name="local"/>,
    /// reporting (bytesDone, bytesTotal) via <paramref name="progress"/>.</summary>
    public void DownloadTo(string remote, string local, Action<long, long>? progress = null)
    {
        var err = default(NativeError);
        var (fp, keepAlive) = MakeProgress(progress);
        int rc = syno_download_to(_client, remote, local, fp, IntPtr.Zero, ref err);
        GC.KeepAlive(keepAlive);
        Check(rc, ref err);
    }

    /// <summary>Upload <paramref name="local"/> into the remote directory
    /// <paramref name="remoteDir"/>. Progress is coarse (0 → total).</summary>
    public void Upload(string local, string remoteDir, bool overwrite = true,
        Action<long, long>? progress = null)
    {
        var err = default(NativeError);
        var (fp, keepAlive) = MakeProgress(progress);
        int rc = syno_upload(_client, local, remoteDir, overwrite, fp, IntPtr.Zero, ref err);
        GC.KeepAlive(keepAlive);
        Check(rc, ref err);
    }

    public void Delete(string path)
    {
        var err = default(NativeError);
        Check(syno_delete(_client, path, ref err), ref err);
    }

    public void CreateFolder(string parent, string name)
    {
        var err = default(NativeError);
        Check(syno_create_folder(_client, parent, name, ref err), ref err);
    }

    public void Rename(string path, string newName)
    {
        var err = default(NativeError);
        Check(syno_rename(_client, path, newName, ref err), ref err);
    }

    // ── Mount lifecycle ──────────────────────────────────────────────────────────

    /// <summary>Mount the share at <paramref name="mountpoint"/> in-process.
    /// Returns once the mount is established (or throws on failure).</summary>
    public void Mount(string mountpoint, ulong cacheTtl, ulong readCacheMb, ulong prefetchBlocks)
    {
        // Tear down any prior mount first so a second Mount() can't orphan the
        // previous native SynoMount (and its background worker / runtime).
        Unmount();
        var err = default(NativeError);
        int rc = syno_mount(
            _client, mountpoint, cacheTtl, readCacheMb, prefetchBlocks, out var mount, ref err);
        Check(rc, ref err);
        _mount = mount;
    }

    /// <summary>Unmount, if mounted. Blocks until the background worker exits.</summary>
    public void Unmount()
    {
        if (_mount != IntPtr.Zero)
        {
            syno_unmount(_mount);
            _mount = IntPtr.Zero;
        }
    }

    // ── Logging bridge ───────────────────────────────────────────────────────────

    // Held to keep the delegate alive for as long as native code may call it.
    private static LogCb? _logKeepAlive;

    /// <summary>Route the library's log lines to <paramref name="onLine"/>
    /// (level, text), or pass null to clear. Installed process-wide.</summary>
    public static void SetLogCallback(Action<int, string>? onLine)
    {
        NativeMethods.EnsureResolver();
        if (onLine is null)
        {
            // Tell native to stop calling FIRST, then drop the managed delegate —
            // otherwise a log line dispatched on a Tokio thread in the gap could
            // invoke a GC-collected delegate.
            syno_set_log_callback(IntPtr.Zero, IntPtr.Zero);
            _logKeepAlive = null;
            return;
        }
        _logKeepAlive = (level, line, _) =>
        {
            // This runs as an unmanaged callback — an exception escaping here
            // would tear down the process. Swallow anything a subscriber throws.
            try
            {
                var text = line != IntPtr.Zero ? Marshal.PtrToStringUTF8(line) ?? "" : "";
                onLine(level, text);
            }
            catch
            {
                // Best-effort logging: never let a log line crash the app.
            }
        };
        syno_set_log_callback(Marshal.GetFunctionPointerForDelegate(_logKeepAlive), IntPtr.Zero);
    }

    /// <summary>Set native log verbosity ("error"/"warn"/"info"/"debug"/"trace").</summary>
    public static void SetLogLevel(string level)
    {
        NativeMethods.EnsureResolver();
        syno_set_log_level(level);
    }

    /// <summary>Also write every log record to <paramref name="path"/>, or stop
    /// writing to a file when it is null. Returns where the log actually
    /// landed, which is worth showing: a relative path is made absolute and
    /// missing parent directories are created.
    ///
    /// The log pane this class already feeds dies with the window. A file does
    /// not, which is the only way to read what a mount was doing when it took
    /// the machine down with it.</summary>
    public static string? SetLogFile(string? path)
    {
        NativeMethods.EnsureResolver();
        var err = default(NativeError);
        var rc = syno_set_log_file(path, out var outPath, ref err);
        Check(rc, ref err);
        return path is null ? null : TakeString(outPath);
    }

    // ── Dispose ────────────────────────────────────────────────────────────────

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        Unmount();
        if (_client != IntPtr.Zero)
        {
            syno_logout(_client);
            syno_client_free(_client);
            _client = IntPtr.Zero;
        }
    }

    // ── Helpers ──────────────────────────────────────────────────────────────────

    private static (IntPtr fp, ProgressCb? keepAlive) MakeProgress(Action<long, long>? progress)
    {
        if (progress is null) return (IntPtr.Zero, null);
        // Invoked as an unmanaged callback — guard so a throwing progress
        // handler can't escape the native boundary and crash the process.
        ProgressCb cb = (done, total, _) =>
        {
            try { progress((long)done, (long)total); }
            catch { /* progress is advisory; never let it abort the transfer */ }
        };
        return (Marshal.GetFunctionPointerForDelegate(cb), cb);
    }

    private static readonly JsonSerializerOptions JsonOpts = new()
    {
        PropertyNameCaseInsensitive = true,
    };

    private static SynoFileInfo[] ParseArray(IntPtr json)
    {
        var s = TakeString(json);
        return JsonSerializer.Deserialize<SynoFileInfo[]>(s, JsonOpts) ?? Array.Empty<SynoFileInfo>();
    }

    private static SynoFileInfo ParseObject(IntPtr json)
    {
        var s = TakeString(json);
        // A successful native call always emits a real object; a null here means
        // the FFI broke its contract, so fail fast rather than hand back an empty
        // record that looks like a real (but blank) entry.
        return JsonSerializer.Deserialize<SynoFileInfo>(s, JsonOpts)
            ?? throw new SynoException(SynoStatus.Io, 0, "native returned malformed file-info JSON");
    }

    /// <summary>Copy a native string to managed and free the native buffer.</summary>
    private static string TakeString(IntPtr ptr)
    {
        if (ptr == IntPtr.Zero) return "";
        var s = Marshal.PtrToStringUTF8(ptr) ?? "";
        syno_string_free(ptr);
        return s;
    }

    /// <summary>Throw a typed exception for any non-OK status, freeing the
    /// native error message first.</summary>
    private static void Check(int rc, ref NativeError err)
    {
        var status = (SynoStatus)rc;
        if (status == SynoStatus.Ok) return;

        var message = err.Message != IntPtr.Zero ? Marshal.PtrToStringUTF8(err.Message) ?? "" : "";
        var dsm = err.DsmCode;
        if (err.Message != IntPtr.Zero)
        {
            syno_string_free(err.Message);
            err.Message = IntPtr.Zero;
        }
        if (message.Length == 0) message = $"native error {rc}";

        throw status switch
        {
            SynoStatus.OtpRequired => new OtpRequiredException(message),
            // Rewritten rather than passed through: the native message says what
            // rustls objected to, which is not what the user needs to know.
            SynoStatus.TlsError => new TlsVerificationException(
                "The NAS's TLS certificate could not be verified. If this NAS uses "
                + "a self-signed certificate, untick \"Verify the NAS TLS certificate\" "
                + "and try again — the connection will be encrypted but not "
                + $"authenticated. ({message})"),
            _ => new SynoException(status, dsm, message),
        };
    }
}
