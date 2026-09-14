using System;
using System.Text.RegularExpressions;
using System.Threading.Tasks;
using SynologyFuse.Gui.Interop;
using SynologyFuse.Gui.Models;

namespace SynologyFuse.Gui.Services;

/// <summary>
/// Drives the native FileStation client directly (no subprocess). Connecting,
/// mounting and unmounting all run in-process via <see cref="SynoClient"/>.
/// Log lines from the native library are surfaced through
/// <see cref="OutputReceived"/>; connect/mount failures surface as
/// <see cref="SynoException"/> (or <see cref="OtpRequiredException"/> when a
/// 2FA code is needed).
/// </summary>
public sealed class MountService : IDisposable
{
    private SynoClient? _client;
    private bool _disposed;

    /// <summary>Raised for each log line emitted by the native library.
    /// May fire on a background thread — marshal to the UI thread in handlers.</summary>
    public event Action<string>? OutputReceived;

    public MountService()
    {
        // Route native log output to subscribers for the GUI log pane.
        SynoClient.SetLogCallback((_, line) => OutputReceived?.Invoke(line));
    }

    public bool IsMounted => _client is { IsMounted: true };

    /// <summary>Point the native logger at this configuration's file, or clear
    /// it, and say in the log pane where the file went.
    ///
    /// A log file that could not be opened must not stop the connection: the
    /// user asked to mount a NAS, not to write a file. It is reported in the
    /// pane and the mount goes ahead without it.</summary>
    private void ApplyLogFile(MountConfig config)
    {
        try
        {
            var landed = SynoClient.SetLogFile(config.LogFile);
            if (landed is not null)
            {
                OutputReceived?.Invoke($"Writing the log to {landed}");
            }
        }
        catch (SynoException ex)
        {
            OutputReceived?.Invoke($"Could not write the log file: {ex.Message}");
        }
    }

    /// <summary>Which leg the last connection reached the NAS by.
    ///
    /// <see cref="SynoTransport.Unknown"/> until something has connected. Worth
    /// showing: the difference between SMB and the HTTP API is the difference
    /// between a transfer that resumes where it stopped and one that starts
    /// again, and nothing else tells a user which they got.</summary>
    public SynoTransport Transport { get; private set; } = SynoTransport.Unknown;

    /// <summary>An empty setting means "not set", which the native side spells
    /// null. A blank string would be a domain of "" and a profile path of "".</summary>
    internal static string? Blank(string value) =>
        string.IsNullOrWhiteSpace(value) ? null : value;

    /// <summary>Where the profile lives on this computer for this connection:
    /// the file the user pointed at, or the copy a download is kept in.</summary>
    internal static string? VpnProfileFor(MountConfig config)
    {
        var resolved = SettingsService.ResolveVpnProfile(
            config.VpnProfile, config.VpnProfileNas, config.VpnHost);
        return resolved is null ? null : ExpandPath(resolved);
    }

    /// <summary>
    /// Connect and mount in one step. Throws <see cref="OtpRequiredException"/>
    /// when the account needs a 2FA code (prompt and retry with
    /// <paramref name="otp"/> set), or <see cref="SynoException"/> on failure.
    /// </summary>
    public async Task ConnectAndMountAsync(MountConfig config, string? otp = null)
    {
        if (_client is not null)
            throw new InvalidOperationException("Already mounted.");

        var mountpoint = ExpandPath(config.Mountpoint);
        SynoClient.SetLogLevel(config.LogLevel);
        ApplyLogFile(config);

        var client = await Task.Run(() =>
        {
            var c = SynoClient.Connect(
                config.Host, config.Port, config.UseHttps, config.Username, config.Password, otp,
                autoRelogin: true, verifySsl: config.VerifySsl,
                domain: Blank(config.Domain),
                vpnProfile: VpnProfileFor(config),
                vpnHost: Blank(config.VpnHost),
                vpnProfileRemote: Blank(config.VpnProfileNas));
            try
            {
                c.Mount(mountpoint, config.CacheTtl, config.ReadCacheMb, config.PrefetchBlocks);
            }
            catch (SynoException ex)
            {
                c.Dispose();
                // The login already succeeded, so this is a mount-side failure —
                // retag it so the UI advises on the mount point instead of the
                // credentials.
                throw new MountFailedException(ex);
            }
            catch
            {
                c.Dispose();
                throw;
            }
            return c;
        });

        // If Dispose() ran (app shutdown) while connect+mount was in flight,
        // tear the freshly-built client down instead of leaking it. The await
        // resumed on the UI thread, so this can't race Dispose().
        if (_disposed)
        {
            client.Dispose();
            return;
        }
        _client = client;
        Transport = client.Transport;
    }

    /// <summary>
    /// Validate credentials by logging in and straight back out — no mount.
    /// Same exception contract as <see cref="ConnectAndMountAsync"/>.
    /// </summary>
    public Task TestConnectionAsync(MountConfig config, string? otp = null)
    {
        SynoClient.SetLogLevel(config.LogLevel);
        ApplyLogFile(config);
        return Task.Run(() =>
        {
            using var client = SynoClient.Connect(
                config.Host, config.Port, config.UseHttps, config.Username, config.Password, otp,
                autoRelogin: true, verifySsl: config.VerifySsl,
                domain: Blank(config.Domain),
                vpnProfile: VpnProfileFor(config),
                vpnHost: Blank(config.VpnHost),
                vpnProfileRemote: Blank(config.VpnProfileNas));
            Transport = client.Transport;
            // Dispose logs out immediately; reaching here means success.
        });
    }

    /// <summary>Unmount and log out, if connected.</summary>
    public void Stop()
    {
        _client?.Dispose();
        _client = null;
    }

    /// <summary>How long a disconnect runs before it is worth saying it is
    /// taking a while.
    ///
    /// Teardown unmounts the volume and then waits for the filesystem's own
    /// workers to finish — a read being retried against a NAS that has stopped
    /// answering can hold that open. Worth <em>reporting</em>; not a reason to
    /// stop waiting. See <see cref="StopAsync"/>.</summary>
    public static readonly TimeSpan SlowStopThreshold = TimeSpan.FromSeconds(20);

    /// <summary>Stop, reporting once it is taking a while.
    ///
    /// It keeps waiting, because giving up on the wait gives up on nothing:
    /// the work owns a native handle and cannot be interrupted from here, so
    /// it carries on regardless — while <see cref="Stop"/> has not yet reached
    /// the line that releases this service's own reference. Reporting
    /// "Disconnected" at that point would re-enable Connect over a service
    /// that still holds a client, and the next attempt would fail with
    /// "Already mounted." for as long as the process lived.
    ///
    /// So a slow teardown changes what is <em>said</em>, not what is true.</summary>
    public async Task StopAsync(Action? onSlow = null) =>
        await NotifyIfSlow(Task.Run(Stop), SlowStopThreshold, onSlow);

    /// <summary>Await <paramref name="work"/>, calling <paramref name="onSlow"/>
    /// if it has not finished within <paramref name="patience"/>.
    ///
    /// Always awaits it in the end, so nothing acts on a half-finished
    /// teardown. Separate from <see cref="StopAsync"/> so the rule can be
    /// tested: this service's constructor talks to the native library, which a
    /// unit test has no business needing.</summary>
    internal static async Task NotifyIfSlow(Task work, TimeSpan patience, Action? onSlow)
    {
        if (await Task.WhenAny(work, Task.Delay(patience)) != work) onSlow?.Invoke();
        await work;
    }

    public void Dispose()
    {
        _disposed = true;
        Stop();
        SynoClient.SetLogCallback(null);
    }

    // ── Path expansion (shell-style conveniences the user expects) ──────────────

    internal static string ExpandPath(string path)
    {
        // Expand ~ to the user's home directory (a direct native call does not
        // get shell expansion).
        if (path == "~" || path.StartsWith("~/") || path.StartsWith(@"~\"))
        {
            var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
            path = home + path[1..];
        }

        return ExpandEnvVars(path);
    }

    private static readonly Regex EnvVarUnix =
        new(@"\$\{([^}]+)\}|\$([A-Za-z_][A-Za-z0-9_]*)", RegexOptions.Compiled);

    internal static string ExpandEnvVars(string path)
    {
        // %VAR% — Windows style (also works on Linux in case someone types it).
        path = Environment.ExpandEnvironmentVariables(path);

        // $VAR / ${VAR} — Unix style (not handled by ExpandEnvironmentVariables on Windows).
        path = EnvVarUnix.Replace(path, m =>
        {
            var name = m.Groups[1].Success ? m.Groups[1].Value : m.Groups[2].Value;
            return Environment.GetEnvironmentVariable(name) ?? m.Value;
        });

        return path;
    }
}
