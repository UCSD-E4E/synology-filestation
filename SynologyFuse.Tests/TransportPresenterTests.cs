using SynologyFuse.Gui.Models;
using SynologyFuse.Gui.Services;
using Xunit;

namespace SynologyFuse.Tests;

/// <summary>
/// The badge that says how the NAS is being reached.
///
/// Asserted on the strings directly, because they are what a user sees and the
/// whole point of the badge is that the difference matters: SMB resumes an
/// interrupted transfer where it stopped, the FileStation API starts it again.
/// Before this existed nothing told anyone which one they had.
/// </summary>
public class TransportPresenterTests
{
    [Fact]
    public void NothingIsSaidBeforeAnythingHasConnected()
    {
        Assert.Equal("", TransportPresenter.Badge(SynoTransport.Unknown));
        Assert.Equal("", TransportPresenter.Detail(SynoTransport.Unknown));
    }

    [Theory]
    [InlineData(SynoTransport.SmbDirect, "SMB")]
    [InlineData(SynoTransport.SmbOverVpn, "SMB via VPN")]
    [InlineData(SynoTransport.Https, "HTTP API")]
    public void EachLegIsNamedInAFewWords(SynoTransport transport, string expected) =>
        Assert.Equal(expected, TransportPresenter.Badge(transport));

    [Fact]
    public void TheSlowLegSaysWhatToDoAboutIt()
    {
        // Landing on the HTTP API is usually one unset field away from not
        // happening: a directory account with no domain cannot authenticate
        // SMB, and outside the NAS's network there is no tunnel without one
        // configured. A badge that only named the leg would leave a user to
        // guess which.
        var detail = TransportPresenter.Detail(SynoTransport.Https);

        Assert.Contains("domain", detail);
        Assert.Contains("VPN", detail);
        Assert.Contains("resum", detail);
    }

    [Fact]
    public void TheSlowLegSaysItWillNotLastIfItNeedNot()
    {
        // A connection on the HTTP API moves to SMB by itself once SMB
        // answers. Without saying so, the natural reading of the slow badge is
        // "disconnect and try again", which interrupts the very transfers the
        // move exists to protect.
        var detail = TransportPresenter.Detail(SynoTransport.Https);

        Assert.Contains("by itself", detail);
    }

    [Fact]
    public void TheTunnelledLegSaysWhatItDidToTheMachine()
    {
        // Which is nothing, and that is the reassurance worth giving: a VPN
        // that reconfigures the machine's routing is a different proposition
        // from one that does not.
        var detail = TransportPresenter.Detail(SynoTransport.SmbOverVpn);

        Assert.Contains("tunnelled", detail);
        Assert.Contains("nothing else on the machine", detail);
    }
}
