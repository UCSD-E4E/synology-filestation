using SynologyFuse.Gui.Models;

namespace SynologyFuse.Gui.Services;

/// <summary>
/// How a transport is described to a user: a badge of two or three words, and
/// the sentence behind it.
///
/// A pure function, like <see cref="ErrorPresenter"/> and for the same reason —
/// the wording is what a user reads, so it is worth testing directly, and doing
/// that through a view model would drag in the native library, the settings
/// file and an update check to assert on a string.
/// </summary>
public static class TransportPresenter
{
    /// <summary>Read at a glance, so: two or three words.</summary>
    public static string Badge(SynoTransport transport) => transport switch
    {
        SynoTransport.SmbDirect => "SMB",
        SynoTransport.SmbOverVpn => "SMB via VPN",
        SynoTransport.Https => "HTTP API",
        _ => "",
    };

    /// <summary>
    /// What the badge means, and — on the slow leg — what to do about it.
    /// Landing there is usually one unset field away from not happening, and a
    /// badge that only named the leg would leave a user to guess which.
    /// </summary>
    public static string Detail(SynoTransport transport) => transport switch
    {
        SynoTransport.SmbDirect =>
            "Transfers go straight to the NAS over SMB, and resume where they stopped.",
        SynoTransport.SmbOverVpn =>
            "The NAS did not answer directly, so this connection is tunnelled — "
            + "raised inside this application, with nothing else on the machine affected.",
        SynoTransport.Https =>
            "SMB could not be reached, so transfers use the FileStation API: slower, "
            + "and an interrupted one starts again rather than resuming. "
            + "The connection moves to SMB by itself once SMB answers. "
            + "An account from a directory the NAS is joined to needs its domain set "
            + "for SMB to authenticate; from outside the NAS's network it also needs a VPN.",
        _ => "",
    };
}
