using System;

namespace SynologyFuse.Gui.Models;

/// <summary>
/// How many SMB connections a mount reads over, as the window and the settings
/// file hold it -- a cleared box is null -- and as the native side takes it,
/// where 0 means the mount's default (four).
///
/// Loss on a home link caps each connection rather than the link, so a lossy
/// link reads faster over several. One is the session alone.
/// </summary>
public static class SmbConnectionsSetting
{
    /// <summary>What to hand <c>syno_connect</c>. Rounded as the box shows it;
    /// anything that is not a count at all is the default rather than an
    /// exception out of the connect button.</summary>
    public static uint ToNative(decimal? value)
    {
        if (value is not decimal v) return 0;
        var whole = Math.Round(v, MidpointRounding.AwayFromZero);
        return whole > 0 && whole <= uint.MaxValue ? (uint)whole : 0;
    }

    /// <summary>What the box shows for a value read from settings: empty for
    /// the default, or for junk a hand edit left in the file.</summary>
    public static decimal? FromPersisted(decimal? value) =>
        ToNative(value) is var native and > 0 ? native : null;
}
