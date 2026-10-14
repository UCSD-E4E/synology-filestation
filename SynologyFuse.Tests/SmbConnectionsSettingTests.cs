using SynologyFuse.Gui.Models;
using Xunit;

namespace SynologyFuse.Tests;

/// <summary>
/// How many SMB connections a mount reads over, as the window holds it (a
/// cleared box is null) and as the native side takes it (0 for the mount's
/// default, four).
/// </summary>
public class SmbConnectionsSettingTests
{
    [Theory]
    [InlineData(null, 0u)]
    [InlineData(0, 0u)]
    [InlineData(1, 1u)]
    [InlineData(4, 4u)]
    [InlineData(8, 8u)]
    public void ToNative_PassesWholeNumbersOnAndMapsEmptyToTheDefault(int? value, uint expected)
    {
        Assert.Equal(expected, SmbConnectionsSetting.ToNative(value));
    }

    /// <summary>Shown as 3 by the box's whole-number format, so that is what is
    /// sent.</summary>
    [Fact]
    public void ToNative_RoundsAFraction_AsTheBoxDisplaysIt()
    {
        Assert.Equal(3u, SmbConnectionsSetting.ToNative(2.6m));
    }

    /// <summary>Not a number of connections at all: the default, not an
    /// exception out of the connect button.</summary>
    [Theory]
    [InlineData(-1)]
    [InlineData(-40)]
    public void ToNative_ANegativeValueMeansTheDefault(int value)
    {
        Assert.Equal(0u, SmbConnectionsSetting.ToNative(value));
    }

    [Fact]
    public void ToNative_AValueTooLargeForTheNativeTypeMeansTheDefault()
    {
        Assert.Equal(0u, SmbConnectionsSetting.ToNative(10_000_000_000m));
    }

    [Theory]
    [InlineData(null, null)]
    [InlineData(0, null)]
    [InlineData(-5, null)]
    [InlineData(2, 2)]
    public void FromPersisted_ShowsTheDefaultAsEmpty(int? persisted, int? shown)
    {
        Assert.Equal((decimal?)shown, SmbConnectionsSetting.FromPersisted(persisted));
    }
}
