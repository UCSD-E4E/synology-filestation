using System;
using System.IO;
using SynologyFuse.Gui.Interop;
using SynologyFuse.Gui.Models;
using SynologyFuse.Gui.Services;
using Xunit;

namespace SynologyFuse.Tests;

public class MountServiceTests
{
    // ── ExpandEnvVars ─────────────────────────────────────────────────────────

    [Fact]
    public void ExpandEnvVars_PercentStyle_Expanded()
    {
        Environment.SetEnvironmentVariable("SYNOTEST_VAR", "expanded");
        try
        {
            var result = MountService.ExpandEnvVars("%SYNOTEST_VAR%/path");
            Assert.Equal("expanded/path", result);
        }
        finally
        {
            Environment.SetEnvironmentVariable("SYNOTEST_VAR", null);
        }
    }

    [Fact]
    public void ExpandEnvVars_DollarStyle_Expanded()
    {
        Environment.SetEnvironmentVariable("SYNOTEST_VAR", "expanded");
        try
        {
            var result = MountService.ExpandEnvVars("/path/$SYNOTEST_VAR/sub");
            Assert.Equal("/path/expanded/sub", result);
        }
        finally
        {
            Environment.SetEnvironmentVariable("SYNOTEST_VAR", null);
        }
    }

    [Fact]
    public void ExpandEnvVars_CurlyBraceStyle_Expanded()
    {
        Environment.SetEnvironmentVariable("SYNOTEST_VAR", "expanded");
        try
        {
            var result = MountService.ExpandEnvVars("/path/${SYNOTEST_VAR}/sub");
            Assert.Equal("/path/expanded/sub", result);
        }
        finally
        {
            Environment.SetEnvironmentVariable("SYNOTEST_VAR", null);
        }
    }

    [Fact]
    public void ExpandEnvVars_UndefinedVar_LeftUnchanged()
    {
        Environment.SetEnvironmentVariable("SYNOTEST_UNDEF", null);
        var result = MountService.ExpandEnvVars("/path/$SYNOTEST_UNDEF/sub");
        Assert.Equal("/path/$SYNOTEST_UNDEF/sub", result);
    }

    [Fact]
    public void ExpandEnvVars_NoVars_Unchanged()
    {
        Assert.Equal("/mnt/share", MountService.ExpandEnvVars("/mnt/share"));
    }

    // ── ExpandPath ────────────────────────────────────────────────────────────

    [Fact]
    public void ExpandPath_TildeAlone_ExpandsToHome()
    {
        var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        Assert.Equal(home, MountService.ExpandPath("~"));
    }

    [Fact]
    public void ExpandPath_TildeSlash_ExpandsToHomeSubdir()
    {
        var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        var result = MountService.ExpandPath("~/mounts/nas");
        Assert.Equal(home + "/mounts/nas", result);
    }

    [Fact]
    public void ExpandPath_NoTilde_Unchanged()
    {
        Assert.Equal("/mnt/nas", MountService.ExpandPath("/mnt/nas"));
    }

    // ── LogFileFor ────────────────────────────────────────────────────────────

    /// <summary>The log path is typed into the same form as the mountpoint,
    /// by somebody who has no shell to expand it for them. Left literal, a
    /// "~/logs/mount.log" becomes a directory actually named "~" under
    /// whatever the GUI's working directory happens to be.</summary>
    [Fact]
    public void LogFileFor_TildePath_IsExpandedLikeEveryOtherPath()
    {
        var home = Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);

        var resolved = MountService.LogFileFor(
            new MountConfig { LogFile = "~/logs/mount.log" });

        Assert.Equal(home + "/logs/mount.log", resolved);
    }

    /// <summary>And it has to be expanded by the same rule the mountpoint is,
    /// or the deadlock guard compares an expanded mountpoint against a literal
    /// log path and waves through exactly the case it exists to catch.</summary>
    [Fact]
    public void LogFileFor_TildePathInsideTheMountpoint_ResolvesUnderIt()
    {
        var config = new MountConfig { Mountpoint = "~/mnt", LogFile = "~/mnt/mount.log" };

        var log = MountService.LogFileFor(config)!;

        Assert.StartsWith(MountService.ExpandPath(config.Mountpoint), log);
    }

    [Fact]
    public void LogFileFor_NoLogFile_IsNull()
    {
        Assert.Null(MountService.LogFileFor(new MountConfig()));
    }

    // ── NativeMethods.FindRepoRoot (native-library resolver) ────────────────────

    [Fact]
    public void FindRepoRoot_FindsCargoTomlInParent()
    {
        var root = Path.Combine(Path.GetTempPath(), Path.GetRandomFileName());
        var subDir = Path.Combine(root, "a", "b", "c");
        Directory.CreateDirectory(subDir);
        File.WriteAllText(Path.Combine(root, "Cargo.toml"), "[package]");
        try
        {
            var found = NativeMethods.FindRepoRoot(subDir);
            Assert.Equal(root, found);
        }
        finally
        {
            Directory.Delete(root, recursive: true);
        }
    }

    [Fact]
    public void FindRepoRoot_CargoTomlInStartDir_ReturnsThatDir()
    {
        var dir = Path.Combine(Path.GetTempPath(), Path.GetRandomFileName());
        Directory.CreateDirectory(dir);
        File.WriteAllText(Path.Combine(dir, "Cargo.toml"), "[package]");
        try
        {
            var found = NativeMethods.FindRepoRoot(dir);
            Assert.Equal(dir, found);
        }
        finally
        {
            Directory.Delete(dir, recursive: true);
        }
    }
}
