using System;
using SynologyFuse.Gui.Models;

namespace SynologyFuse.Gui.Services;

/// <summary>
/// Re-reads which leg a connection is on, and says when it has changed.
///
/// The leg is no longer settled at connect: a connection that started on the
/// HTTP API moves to SMB when SMB becomes reachable, one through the tunnel
/// moves to a direct connection when that answers, and one whose SMB session
/// dies is back on HTTP until it returns. Reading it is cheap — the native
/// side never touches the network to answer — so the window simply asks again
/// every <see cref="Interval"/>.
///
/// Separate from the view model so the rule for what counts as a change can be
/// tested without the native library behind it.
/// </summary>
public sealed class TransportWatch
{
    /// <summary>How often the badge is re-read. The native side looks for a
    /// better leg once a minute; this only has to notice within a few seconds
    /// of it happening.</summary>
    public static readonly TimeSpan Interval = TimeSpan.FromSeconds(5);

    private readonly Func<SynoTransport> _read;
    private SynoTransport _last;

    public TransportWatch(Func<SynoTransport> read, SynoTransport initial)
    {
        _read = read;
        _last = initial;
    }

    /// <summary>The leg now, when it differs from the one last seen; otherwise
    /// null. <see cref="SynoTransport.Unknown"/> is never a change: it is what
    /// a released handle answers, not a leg anything moved to.</summary>
    public SynoTransport? Poll()
    {
        var now = _read();
        if (now == SynoTransport.Unknown || now == _last)
        {
            return null;
        }
        _last = now;
        return now;
    }
}
