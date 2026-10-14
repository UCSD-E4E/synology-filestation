using System;
using SynologyFuse.Gui.Interop;
using static SynologyFuse.Gui.Interop.NativeMethods;

namespace SynologyFuse.Gui.Services;

/// <summary>
/// What to tell the user about a failure: a one-line <see cref="Title"/> naming
/// what went wrong, a <see cref="Remedy"/> naming what to do about it, and the
/// raw <see cref="Detail"/> (native message plus DSM code) for a bug report.
/// </summary>
/// <param name="Title">Plain-language summary — the banner headline.</param>
/// <param name="Remedy">The action that fixes it, in the vocabulary of this
/// window's fields and of DSM's own menus.</param>
/// <param name="Detail">Raw underlying message; may be empty.</param>
public sealed record ErrorReport(string Title, string Remedy, string Detail);

/// <summary>
/// Turns a <see cref="SynoException"/> into advice.
///
/// The native layer already classifies every failure (<see cref="SynoStatus"/>
/// plus the raw DSM code — see <c>classify</c> in the FFI crate); this is the
/// last step, mapping that classification to words a user can act on. Keeping
/// it a pure function of the exception is deliberate: the wording is the part
/// worth testing, and it stays testable without a NAS or the native library.
/// </summary>
public static class ErrorPresenter
{
    public static ErrorReport Describe(Exception ex)
    {
        // Volume failures come after a successful login, so none of the
        // credential advice applies — check before the status switch.
        if (ex is MountFailedException mount)
        {
            return new ErrorReport(
                "Signed in, but the volume could not be opened",
                "The NAS accepted your credentials — it is the local volume that failed. "
                + "Check that the volume location exists, is empty and is writable, and "
                + "that the filesystem driver is installed (FUSE on Linux, WinFsp on Windows).",
                // The wrapper copies the underlying status, code and message, so
                // this is the mount call's own failure, not a login one.
                Detail(mount));
        }

        if (ex is not SynoException syno)
        {
            return new ErrorReport(
                "Something went wrong",
                "See the details below, and the log pane for the full output.",
                ex.Message);
        }

        var (title, remedy) = Advise(syno);
        return new ErrorReport(title, remedy, Detail(syno));
    }

    private static (string Title, string Remedy) Advise(SynoException ex) => ex.Status switch
    {
        // Refused before any login, so "the NAS rejected the request" would send
        // the user to the wrong place. The native message carries the range,
        // and stays in the detail beneath this.
        SynoStatus.InvalidArg when ex.Message.Contains("tunnel MTU", StringComparison.Ordinal) => (
            "The VPN MTU is out of range",
            "Set VPN MTU to a value in the range given below, or clear it to use the "
            + "default."),

        // A rejected login is the common case and the one worth being specific
        // about: DSM says exactly why, and each cause has a different fix.
        SynoStatus.LoginFailed => LoginAdvice(ex.DsmCode),

        SynoStatus.OtpRequired => (
            "A two-factor code is required",
            "Enter the current code from your authenticator app to finish signing in."),

        SynoStatus.TlsError => (
            "The NAS's certificate could not be verified",
            "A DSM appliance ships with a self-signed certificate, which nothing outside "
            + "the NAS can vouch for. Untick \"Verify the NAS TLS certificate\" to connect "
            + "anyway — the connection stays encrypted, but nothing proves you are talking "
            + "to your NAS — or install a trusted certificate on the NAS."),

        SynoStatus.Io => (
            "Could not reach the NAS",
            "Check the host name or IP and the port (DSM listens on 5000 for HTTP and 5001 "
            + "for HTTPS), that the NAS is powered on, and that you are on the same network "
            + "or connected to the VPN."),

        SynoStatus.SidNotFound => (
            "The NAS session expired",
            "The NAS no longer recognises this session. Connect again to start a new one."),

        SynoStatus.PermissionDenied => (
            "The account does not have permission for that",
            "Grant this user access to the shared folder in DSM → Control Panel → "
            + "Shared Folder → Edit → Permissions."),

        SynoStatus.NotFound => (
            "The NAS could not find that path",
            "Check the shared folder still exists and that this account can see it."),

        SynoStatus.AlreadyExists => (
            "That name is already taken on the NAS",
            "Pick a different name, or remove the existing entry first."),

        SynoStatus.NotEmpty => (
            "That folder is not empty",
            "Empty the folder on the NAS before removing it."),

        SynoStatus.NoSpace => (
            "The NAS is out of space or quota",
            "Free space on the volume, or raise this user's quota in DSM → Control Panel → "
            + "User & Group."),

        SynoStatus.InvalidArg => (
            "The NAS rejected the request",
            "Check the values in this window — a path or option was not accepted."),

        SynoStatus.NotSupported => (
            "The NAS does not support that operation",
            "This is a FileStation limitation, not a settings problem. See the details below."),

        // Busy is the one DSM code worth calling out here: it means "come back
        // later", which is very different advice from everything else.
        SynoStatus.Api when ex.DsmCode == 402 => (
            "The NAS is busy",
            "DSM is refusing new requests for the moment. Wait a few seconds and try again."),

        SynoStatus.Api => (
            "The NAS reported an error",
            "See the DSM error code in the details below; the log pane has the full exchange."),

        SynoStatus.NullArg or SynoStatus.Panic => (
            "Internal error in the FileStation driver",
            "This is a bug in this application, not a setting you can fix. Please report it "
            + "with the log output."),

        _ => (
            "Something went wrong",
            "See the details below, and the log pane for the full output."),
    };

    /// <summary>
    /// Advice for a rejected login, keyed by DSM's authentication error codes.
    /// These are the <c>SYNO.API.Auth</c> codes, which overlap numerically with
    /// FileStation's own codes but mean different things — the native layer
    /// keeps them apart by reporting <see cref="SynoStatus.LoginFailed"/>, so
    /// this table is only ever consulted for a login.
    /// </summary>
    private static (string, string) LoginAdvice(uint dsmCode) => dsmCode switch
    {
        400 => (
            "Wrong username or password",
            "Check the username and password and try again. Repeated failures make DSM "
            + "block this computer for a while."),

        401 => (
            "That account is disabled",
            "Re-enable the account in DSM → Control Panel → User & Group, or sign in as "
            + "another user."),

        402 => (
            "That account may not sign in here",
            "DSM denied permission for this login. Check the account's application "
            + "privileges in DSM → Control Panel → User & Group → Edit → Applications."),

        403 or 406 => (
            "A two-factor code is required",
            "This account has two-factor authentication enabled. Enter the current code "
            + "from your authenticator app."),

        404 => (
            "The two-factor code was not accepted",
            "Codes expire after about 30 seconds. Check the clock on the device generating "
            + "them and enter the next code."),

        407 => (
            "This computer's IP address is blocked",
            "DSM auto-blocked this address after repeated failed sign-ins. Remove it from "
            + "DSM → Control Panel → Security → Account → Block List, or wait for the block "
            + "to lapse."),

        408 or 409 or 410 => (
            "The account's password has expired",
            "Sign in to DSM in a browser, set a new password, then connect again."),

        411 => (
            "The account is locked",
            "DSM has locked this account. An administrator can unlock it in DSM → Control "
            + "Panel → User & Group."),

        _ => (
            "The NAS rejected the login",
            "Check the username, password and — if the account uses two-factor "
            + "authentication — the code. The DSM error code is in the details below."),
    };

    /// <summary>Raw message plus the DSM code, when there is one. This is the
    /// line a user copies into a bug report; it is never the only thing shown.</summary>
    private static string Detail(SynoException ex) =>
        ex.DsmCode == 0
            ? ex.Message
            : $"{ex.Message} (DSM code {ex.DsmCode})";
}
