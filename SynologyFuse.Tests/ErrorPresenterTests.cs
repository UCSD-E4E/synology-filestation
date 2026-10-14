using System;
using SynologyFuse.Gui.Interop;
using SynologyFuse.Gui.Services;
using Xunit;
using static SynologyFuse.Gui.Interop.NativeMethods;

namespace SynologyFuse.Tests;

/// <summary>
/// Covers the failure → user-facing-advice mapping. These are the strings the
/// error banner shows, so they are asserted on directly: a user who cannot
/// connect must be told which field to fix, not handed a native error string.
/// </summary>
public class ErrorPresenterTests
{
    private static ErrorReport Describe(SynoStatus status, uint dsm, string message = "native says so") =>
        ErrorPresenter.Describe(new SynoException(status, dsm, message));

    // ── Login failures: the DSM auth codes ────────────────────────────────────

    [Fact]
    public void BadCredentials_PointsAtUsernameAndPassword()
    {
        var r = Describe(SynoStatus.LoginFailed, 400);

        Assert.Contains("password", r.Title, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("username", r.Remedy, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void DisabledAccount_SaysTheAccountIsDisabled()
    {
        var r = Describe(SynoStatus.LoginFailed, 401);

        Assert.Contains("disabled", r.Title, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void RejectedOtp_SaysTheCodeWasRejected()
    {
        var r = Describe(SynoStatus.LoginFailed, 404);

        Assert.Contains("code", r.Title, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("expire", r.Remedy, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void BlockedIp_NamesTheDsmBlockList()
    {
        var r = Describe(SynoStatus.LoginFailed, 407);

        Assert.Contains("blocked", r.Title, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("Block List", r.Remedy, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void ExpiredPassword_TellsTheUserToChangeItInDsm()
    {
        var r = Describe(SynoStatus.LoginFailed, 409);

        Assert.Contains("expired", r.Title, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("DSM", r.Remedy, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void LockedAccount_SaysItIsLocked()
    {
        var r = Describe(SynoStatus.LoginFailed, 411);

        Assert.Contains("locked", r.Title, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void UnknownLoginCode_StillSaysTheLoginWasRejected()
    {
        var r = Describe(SynoStatus.LoginFailed, 499);

        Assert.Contains("login", r.Title, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("499", r.Detail);
    }

    // ── Transport / TLS / session ─────────────────────────────────────────────

    [Fact]
    public void Unreachable_PointsAtHostAndPort()
    {
        var r = Describe(SynoStatus.Io, 0, "connection refused");

        Assert.Contains("reach", r.Title, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("host", r.Remedy, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("port", r.Remedy, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("connection refused", r.Detail);
    }

    [Fact]
    public void TlsFailure_OffersTheSelfSignedRemedy()
    {
        var r = ErrorPresenter.Describe(new TlsVerificationException("certificate not trusted"));

        Assert.Contains("certificate", r.Title, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("Verify the NAS TLS certificate", r.Remedy);
    }

    [Fact]
    public void ExpiredSession_TellsTheUserToConnectAgain()
    {
        var r = Describe(SynoStatus.SidNotFound, 119);

        Assert.Contains("session", r.Title, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("again", r.Remedy, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void OtpRequired_AsksForTheCode()
    {
        var r = ErrorPresenter.Describe(new OtpRequiredException("otp required"));

        Assert.Contains("two-factor", r.Title, StringComparison.OrdinalIgnoreCase);
    }

    // ── NAS-side errors ───────────────────────────────────────────────────────

    [Fact]
    public void BusyNas_SaysToRetry()
    {
        var r = Describe(SynoStatus.Api, 402);

        Assert.Contains("busy", r.Title, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("again", r.Remedy, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void PermissionDenied_PointsAtSharedFolderPermissions()
    {
        var r = Describe(SynoStatus.PermissionDenied, 408);

        Assert.Contains("permission", r.Title, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("shared folder", r.Remedy, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void UnmappedDsmCode_ShowsTheCodeInTheDetail()
    {
        var r = Describe(SynoStatus.Api, 1234, "upload failed");

        Assert.Contains("1234", r.Detail);
        Assert.Contains("upload failed", r.Detail);
    }

    // ── Volume failures (connected, but the volume itself failed to open) ─────

    [Fact]
    public void VolumeFailure_PointsAtTheVolumeLocationRatherThanTheLogin()
    {
        var inner = new SynoException(SynoStatus.Io, 0, "mkdir /mnt/nas: permission denied");
        var r = ErrorPresenter.Describe(new MountFailedException(inner));

        Assert.Contains("volume", r.Title, StringComparison.OrdinalIgnoreCase);
        // Names the field the user has to fix, in the words the form uses for it.
        Assert.Contains("volume location", r.Remedy, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("mkdir /mnt/nas", r.Detail);
        // The credentials were accepted — saying "check your password" here would
        // send the user to the wrong field.
        Assert.DoesNotContain("password", r.Remedy, StringComparison.OrdinalIgnoreCase);
    }

    [Fact]
    public void VolumeFailure_ChainsTheUnderlyingFailureAsInnerException()
    {
        // Generic logging and the debugger both walk InnerException; leaving the
        // cause only on a bespoke property would drop the original stack trace.
        var inner = new SynoException(SynoStatus.Io, 0, "mkdir /mnt/nas: permission denied");

        var wrapped = new MountFailedException(inner);

        Assert.Same(inner, wrapped.InnerException);
        Assert.Equal(inner.DsmCode, wrapped.DsmCode);
    }

    // ── Fallbacks and invariants ──────────────────────────────────────────────

    [Fact]
    public void PlainException_StillProducesAReport()
    {
        var r = ErrorPresenter.Describe(new InvalidOperationException("Already mounted."));

        Assert.False(string.IsNullOrWhiteSpace(r.Title));
        Assert.Contains("Already mounted.", r.Detail);
    }

    [Fact]
    public void NoDsmCode_LeavesTheDetailFreeOfACodeSuffix()
    {
        var r = Describe(SynoStatus.Io, 0, "connection reset");

        Assert.Equal("connection reset", r.Detail);
    }

    [Theory]
    [InlineData((int)SynoStatus.NotFound)]
    [InlineData((int)SynoStatus.PermissionDenied)]
    [InlineData((int)SynoStatus.AlreadyExists)]
    [InlineData((int)SynoStatus.NotEmpty)]
    [InlineData((int)SynoStatus.InvalidArg)]
    [InlineData((int)SynoStatus.NoSpace)]
    [InlineData((int)SynoStatus.NotSupported)]
    [InlineData((int)SynoStatus.Io)]
    [InlineData((int)SynoStatus.Api)]
    [InlineData((int)SynoStatus.LoginFailed)]
    [InlineData((int)SynoStatus.OtpRequired)]
    [InlineData((int)SynoStatus.NullArg)]
    [InlineData((int)SynoStatus.Panic)]
    [InlineData((int)SynoStatus.SidNotFound)]
    [InlineData((int)SynoStatus.TlsError)]
    public void EveryStatus_HasATitleAndARemedy(int status)
    {
        var r = Describe((SynoStatus)status, 0);

        Assert.False(string.IsNullOrWhiteSpace(r.Title));
        Assert.False(string.IsNullOrWhiteSpace(r.Remedy));
    }

    // ── A VPN MTU out of range ────────────────────────────────────────────────

    /// <summary>Refused by the native side before any login, so "the NAS
    /// rejected the request" would send the user to the wrong place. The
    /// native message carries the range, and stays in the detail.</summary>
    [Fact]
    public void AnMtuOutOfRange_PointsAtTheVpnMtuField()
    {
        const string native = "a tunnel MTU of 500 is outside 576..=1298: smaller than 576 is "
            + "not carried by every path";
        var r = Describe(SynoStatus.InvalidArg, 0, native);

        Assert.Contains("VPN MTU", r.Title);
        Assert.Contains("VPN MTU", r.Remedy);
        Assert.DoesNotContain("NAS rejected", r.Title);
        Assert.Contains("576..=1298", r.Detail);
    }

    [Fact]
    public void AnyOtherInvalidArg_KeepsTheGeneralAdvice()
    {
        var r = Describe(SynoStatus.InvalidArg, 0, "path must be absolute");

        Assert.DoesNotContain("MTU", r.Title);
    }
}
