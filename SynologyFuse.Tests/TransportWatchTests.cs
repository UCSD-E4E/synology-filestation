using SynologyFuse.Gui.Models;
using SynologyFuse.Gui.Services;
using Xunit;

namespace SynologyFuse.Tests;

/// <summary>
/// The badge used to be read once, at connect, and never again — so a
/// connection that moved from the HTTP API to SMB while it ran went on saying
/// "HTTP API", and the badge is the only thing that tells a user whether an
/// interrupted transfer will resume.
/// </summary>
public class TransportWatchTests
{
    [Fact]
    public void A_leg_that_has_not_changed_is_not_news()
    {
        var watch = new TransportWatch(() => SynoTransport.Https, SynoTransport.Https);

        Assert.Null(watch.Poll());
        Assert.Null(watch.Poll());
    }

    [Fact]
    public void A_move_to_a_better_leg_is_reported_once()
    {
        var live = SynoTransport.Https;
        var watch = new TransportWatch(() => live, SynoTransport.Https);

        live = SynoTransport.SmbDirect;

        Assert.Equal(SynoTransport.SmbDirect, watch.Poll());
        Assert.Null(watch.Poll());
    }

    [Fact]
    public void A_fall_back_to_http_is_reported_too()
    {
        // A session that dies puts the data back on the HTTP API. A badge
        // still saying "SMB" then would promise a resume that will not happen.
        var live = SynoTransport.SmbOverVpn;
        var watch = new TransportWatch(() => live, SynoTransport.SmbOverVpn);

        live = SynoTransport.Https;

        Assert.Equal(SynoTransport.Https, watch.Poll());
    }

    [Fact]
    public void A_handle_that_is_gone_changes_nothing()
    {
        // Read after the native client is released, the answer is "unknown",
        // which is not a leg the connection moved to.
        var live = SynoTransport.SmbDirect;
        var watch = new TransportWatch(() => live, SynoTransport.SmbDirect);

        live = SynoTransport.Unknown;

        Assert.Null(watch.Poll());
    }
}
