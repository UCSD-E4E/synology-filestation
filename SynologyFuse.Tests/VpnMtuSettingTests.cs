using SynologyFuse.Gui.Models;
using Xunit;

namespace SynologyFuse.Tests;

/// <summary>
/// The VPN MTU as the window holds it (a cleared box is null) and as the native
/// side takes it (0 for the tunnel's own default). The range is the native
/// side's to check: it refuses with the range in the message, and keeping a
/// copy of those numbers here is how they would drift.
/// </summary>
public class VpnMtuSettingTests
{
    [Theory]
    [InlineData(null, 0u)]
    [InlineData(0, 0u)]
    [InlineData(1150, 1150u)]
    // Out of the tunnel's range but a whole number: passed on, so the native
    // side refuses it and says what the range is.
    [InlineData(500, 500u)]
    public void ToNative_PassesWholeNumbersOnAndMapsEmptyToTheDefault(int? value, uint expected)
    {
        Assert.Equal(expected, VpnMtuSetting.ToNative(value));
    }

    /// <summary>Shown as 1151 by the box's whole-number format, so that is
    /// what is sent.</summary>
    [Fact]
    public void ToNative_RoundsAFraction_AsTheBoxDisplaysIt()
    {
        Assert.Equal(1151u, VpnMtuSetting.ToNative(1150.7m));
    }

    /// <summary>Not representable as a packet size at all: the default, not an
    /// <see cref="System.OverflowException"/> out of the connect button.</summary>
    [Theory]
    [InlineData(-1)]
    [InlineData(-1300)]
    public void ToNative_ANegativeValueMeansTheDefault(int value)
    {
        Assert.Equal(0u, VpnMtuSetting.ToNative(value));
    }

    [Fact]
    public void ToNative_AValueTooLargeForTheNativeTypeMeansTheDefault()
    {
        Assert.Equal(0u, VpnMtuSetting.ToNative(10_000_000_000m));
    }

    /// <summary>What a settings file says, as the box should show it: empty
    /// for the default or for junk a hand edit left there.</summary>
    [Theory]
    [InlineData(null, null)]
    [InlineData(0, null)]
    [InlineData(-5, null)]
    [InlineData(1150, 1150)]
    public void FromPersisted_ShowsTheDefaultAsEmpty(int? persisted, int? shown)
    {
        Assert.Equal((decimal?)shown, VpnMtuSetting.FromPersisted(persisted));
    }
}
