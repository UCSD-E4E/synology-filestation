using System;
using System.IO;
using System.Reflection;
using System.Runtime.InteropServices;

namespace SynologyFuse.Gui.Interop;

/// <summary>
/// P/Invoke surface for the <c>synology_filestation_ffi</c> native library
/// (the Rust C ABI in <c>rust/synology-filestation-ffi</c>). Strings cross the
/// boundary as UTF-8; errors come back through a <see cref="NativeError"/>
/// out-param whose <see cref="NativeError.Message"/> must be freed with
/// <see cref="syno_string_free"/>.
/// </summary>
internal static partial class NativeMethods
{
    /// <summary>Base name; the OS loader / our resolver maps it to
    /// lib&lt;name&gt;.so / &lt;name&gt;.dll / lib&lt;name&gt;.dylib.</summary>
    internal const string Lib = "synology_filestation_ffi";

    /// <summary>Mirrors the Rust <c>SynoStatus</c> enum.</summary>
    internal enum SynoStatus
    {
        Ok = 0,
        NotFound = 1,
        PermissionDenied = 2,
        AlreadyExists = 3,
        NotEmpty = 4,
        InvalidArg = 5,
        NoSpace = 6,
        NotSupported = 7,
        Io = 8,
        Api = 9,
        LoginFailed = 10,
        OtpRequired = 11,
        NullArg = 12,
        Panic = 13,
        SidNotFound = 14,
        TlsError = 15,
    }

    /// <summary>Blittable mirror of the Rust <c>SynoError</c> struct.</summary>
    [StructLayout(LayoutKind.Sequential)]
    internal struct NativeError
    {
        public int Kind;
        public uint DsmCode;
        public IntPtr Message;
    }

    /// <summary>Progress callback: (bytesDone, bytesTotal, userData).</summary>
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate void ProgressCb(ulong done, ulong total, IntPtr userData);

    /// <summary>Log callback: (level, line, userData). level: 1=ERROR…5=TRACE.</summary>
    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    internal delegate void LogCb(int level, IntPtr line, IntPtr userData);

    // ── Session lifecycle ──────────────────────────────────────────────────────

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int syno_connect(
        string host, ushort port, [MarshalAs(UnmanagedType.U1)] bool https,
        string username, string password, string? otp,
        [MarshalAs(UnmanagedType.U1)] bool autoRelogin,
        [MarshalAs(UnmanagedType.U1)] bool verifySsl,
        string? domain, string? vpnProfile, string? vpnHost, string? vpnProfileRemote,
        out IntPtr outClient, ref NativeError err);

    /// <summary>Which leg the connection is using: 0 HTTP, 1 SMB through a
    /// tunnel, 2 SMB direct, -1 for a null handle. Settled at connect time.</summary>
    [LibraryImport(Lib)]
    internal static partial int syno_transport(IntPtr client);

    [LibraryImport(Lib)]
    internal static partial void syno_logout(IntPtr client);

    [LibraryImport(Lib)]
    internal static partial void syno_client_free(IntPtr client);

    [LibraryImport(Lib)]
    internal static partial void syno_string_free(IntPtr s);

    // ── Browse ─────────────────────────────────────────────────────────────────

    [LibraryImport(Lib)]
    internal static partial int syno_list_shares(IntPtr client, out IntPtr outJson, ref NativeError err);

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int syno_list_dir(IntPtr client, string path, out IntPtr outJson, ref NativeError err);

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int syno_get_info(IntPtr client, string path, out IntPtr outJson, ref NativeError err);

    // ── Transfers ──────────────────────────────────────────────────────────────
    // Callbacks are passed as function pointers (nint); pass IntPtr.Zero for none.

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int syno_download_to(
        IntPtr client, string remote, string local, IntPtr progress, IntPtr userData, ref NativeError err);

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int syno_upload(
        IntPtr client, string local, string remoteDir, [MarshalAs(UnmanagedType.U1)] bool overwrite,
        IntPtr progress, IntPtr userData, ref NativeError err);

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int syno_delete(IntPtr client, string path, ref NativeError err);

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int syno_create_folder(IntPtr client, string parent, string name, ref NativeError err);

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int syno_rename(IntPtr client, string path, string newName, ref NativeError err);

    // ── Mount lifecycle ────────────────────────────────────────────────────────

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int syno_mount(
        IntPtr client, string mountpoint, ulong cacheTtl, ulong readCacheMb,
        ulong prefetchBlocks, out IntPtr outMount, ref NativeError err);

    [LibraryImport(Lib)]
    internal static partial void syno_unmount(IntPtr mount);

    // ── Logging ────────────────────────────────────────────────────────────────

    [LibraryImport(Lib)]
    internal static partial void syno_set_log_callback(IntPtr cb, IntPtr userData);

    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial void syno_set_log_level(string level);

    /// <summary>Also write every log record to <paramref name="path"/>, or stop
    /// doing so when it is null. <paramref name="outPath"/> receives where the
    /// file actually landed — a relative path is made absolute and parents are
    /// created, so it is not necessarily what was asked for.</summary>
    [LibraryImport(Lib, StringMarshalling = StringMarshalling.Utf8)]
    internal static partial int syno_set_log_file(
        string? path, out IntPtr outPath, ref NativeError err);

    // ── Native library resolution ──────────────────────────────────────────────

    private static bool _resolverRegistered;
    private static readonly object _gate = new();

    /// <summary>
    /// Register a resolver that finds the native library next to the GUI, then
    /// in a dev build tree, honouring a <c>SYNOFS_NATIVE_DIR</c> override.
    /// Idempotent and called from <see cref="Services.SynoClient"/>'s static ctor.
    /// </summary>
    internal static void EnsureResolver()
    {
        lock (_gate)
        {
            if (_resolverRegistered) return;
            NativeLibrary.SetDllImportResolver(typeof(NativeMethods).Assembly, Resolve);
            _resolverRegistered = true;
        }
    }

    private static IntPtr Resolve(string libraryName, Assembly assembly, DllImportSearchPath? searchPath)
    {
        if (libraryName != Lib)
            return IntPtr.Zero; // not ours — fall back to default resolution

        foreach (var candidate in CandidatePaths())
        {
            if (File.Exists(candidate) && NativeLibrary.TryLoad(candidate, out var handle))
                return handle;
        }

        // Last resort: let the OS resolve by base name (e.g. installed in a
        // standard library directory).
        return NativeLibrary.TryLoad(Lib, assembly, searchPath, out var fallback)
            ? fallback
            : IntPtr.Zero;
    }

    private static string FileName()
    {
        if (RuntimeInformation.IsOSPlatform(OSPlatform.Windows)) return $"{Lib}.dll";
        if (RuntimeInformation.IsOSPlatform(OSPlatform.OSX)) return $"lib{Lib}.dylib";
        return $"lib{Lib}.so";
    }

    private static System.Collections.Generic.IEnumerable<string> CandidatePaths()
    {
        var file = FileName();

        // 1. Explicit override (used by the Nix wrapper and tests).
        var overrideDir = Environment.GetEnvironmentVariable("SYNOFS_NATIVE_DIR");
        if (!string.IsNullOrEmpty(overrideDir))
            yield return Path.Combine(overrideDir, file);

        // 2. Beside the GUI assembly (deployed layout).
        var baseDir = AppContext.BaseDirectory;
        yield return Path.Combine(baseDir, file);

        // 3. Development layout: <repo>/target/{release,debug}.
        var repoRoot = FindRepoRoot(baseDir);
        if (repoRoot is not null)
        {
            yield return Path.Combine(repoRoot, "target", "release", file);
            yield return Path.Combine(repoRoot, "target", "debug", file);
        }
    }

    internal static string? FindRepoRoot(string startDir)
    {
        var dir = new DirectoryInfo(startDir);
        while (dir is not null)
        {
            if (File.Exists(Path.Combine(dir.FullName, "Cargo.toml")))
                return dir.FullName;
            dir = dir.Parent;
        }
        return null;
    }
}
