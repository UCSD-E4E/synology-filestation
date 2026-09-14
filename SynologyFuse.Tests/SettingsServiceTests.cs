using System;
using System.IO;
using System.Text.Json;
using SynologyFuse.Gui.Services;
using Xunit;

namespace SynologyFuse.Tests;

public class SettingsServiceTests : IDisposable
{
    private readonly string _dir;
    private readonly string _path;

    public SettingsServiceTests()
    {
        _dir = Path.Combine(Path.GetTempPath(), Path.GetRandomFileName());
        Directory.CreateDirectory(_dir);
        _path = Path.Combine(_dir, "settings.json");
    }

    public void Dispose() => Directory.Delete(_dir, recursive: true);

    // ── Load ──────────────────────────────────────────────────────────────────

    [Fact]
    public void Load_FileDoesNotExist_ReturnsDefaults()
    {
        var s = SettingsService.Load(_path);

        Assert.Equal("", s.Host);
        Assert.Equal("", s.Username);
        Assert.Equal(5001m, s.Port);
        Assert.True(s.UseHttps);
        Assert.True(s.VerifySsl);
        Assert.Equal("info", s.LogLevel);
    }

    /// <summary>
    /// A settings.json written before certificate verification existed has no
    /// "VerifySsl" key. It must load as verifying — <c>default(bool)</c> is
    /// false, which would silently leave upgraded users on the insecure path
    /// that this option exists to end.
    /// </summary>
    [Fact]
    public void Load_JsonPredatingTheOption_DefaultsToVerifying()
    {
        File.WriteAllText(_path, """
        {
          "Host": "nas.local",
          "Username": "alice",
          "Port": 5001,
          "UseHttps": true,
          "Mountpoint": "/mnt/nas",
          "LogLevel": "info"
        }
        """);

        var s = SettingsService.Load(_path);

        Assert.Equal("nas.local", s.Host);
        Assert.True(s.VerifySsl, "an upgraded settings file must not silently disable verification");
    }

    [Fact]
    public void SaveThenLoad_RoundTripsVerifySsl()
    {
        SettingsService.Save(new PersistedSettings { Host = "nas.local", VerifySsl = false }, _path);

        Assert.False(SettingsService.Load(_path).VerifySsl);
    }

    [Fact]
    public void Load_ValidJson_ReturnsPersistedValues()
    {
        File.WriteAllText(_path, """
            {
                "Host": "nas.example.com",
                "Username": "bob",
                "Port": 5000,
                "UseHttps": false,
                "Mountpoint": "/mnt/nas",
                "CacheTtl": 60,
                "ReadCacheMb": 512,
                "PrefetchBlocks": 4,
                "LogLevel": "debug"
            }
            """);

        var s = SettingsService.Load(_path);

        Assert.Equal("nas.example.com", s.Host);
        Assert.Equal("bob", s.Username);
        Assert.Equal(5000m, s.Port);
        Assert.False(s.UseHttps);
        Assert.Equal("/mnt/nas", s.Mountpoint);
        Assert.Equal(60m, s.CacheTtl);
        Assert.Equal(512m, s.ReadCacheMb);
        Assert.Equal(4m, s.PrefetchBlocks);
        Assert.Equal("debug", s.LogLevel);
    }

    /// <summary>A settings file written before the prefetch knob existed must
    /// keep the behaviour it had, not silently switch speculation off.</summary>
    [Fact]
    public void Load_SettingsWithoutPrefetchBlocks_KeepsTheDefaultWindow()
    {
        File.WriteAllText(_path, """
            {
                "Host": "nas.example.com",
                "Mountpoint": "/mnt/nas"
            }
            """);

        var s = SettingsService.Load(_path);

        Assert.Equal(16m, s.PrefetchBlocks);
    }

    [Fact]
    public void Load_CorruptJson_ReturnsDefaults()
    {
        File.WriteAllText(_path, "{ this is not valid json }}}");

        var s = SettingsService.Load(_path);

        Assert.Equal("", s.Host);
        Assert.Equal("info", s.LogLevel);
    }

    [Fact]
    public void Load_PartialJson_MissingFieldsUseDefaults()
    {
        File.WriteAllText(_path, """{"Host": "partial.host"}""");

        var s = SettingsService.Load(_path);

        Assert.Equal("partial.host", s.Host);
        Assert.Equal("", s.Username);   // default
        Assert.Equal("info", s.LogLevel); // default
    }

    // ── Save ──────────────────────────────────────────────────────────────────

    [Fact]
    public void Save_WritesValidJson()
    {
        var settings = new PersistedSettings
        {
            Host = "mynas.local",
            Username = "alice",
            Port = 5001m,
            UseHttps = true,
            Mountpoint = "/mnt/mynas",
            CacheTtl = 45m,
            ReadCacheMb = 128m,
            PrefetchBlocks = 8m,
            LogLevel = "warn",
        };

        SettingsService.Save(settings, _path);

        Assert.True(File.Exists(_path));
        var json = File.ReadAllText(_path);
        var doc = JsonDocument.Parse(json);
        var root = doc.RootElement;
        Assert.Equal("mynas.local", root.GetProperty("Host").GetString());
        Assert.Equal("alice", root.GetProperty("Username").GetString());
        Assert.Equal(5001m, root.GetProperty("Port").GetDecimal());
        Assert.True(root.GetProperty("UseHttps").GetBoolean());
        Assert.Equal(45m, root.GetProperty("CacheTtl").GetDecimal());
        Assert.Equal(128m, root.GetProperty("ReadCacheMb").GetDecimal());
        Assert.Equal(8m, root.GetProperty("PrefetchBlocks").GetDecimal());
        Assert.Equal("warn", root.GetProperty("LogLevel").GetString());
    }

    [Fact]
    public void Save_CreatesParentDirectories()
    {
        var deepPath = Path.Combine(_dir, "nested", "deep", "settings.json");
        SettingsService.Save(new PersistedSettings(), deepPath);
        Assert.True(File.Exists(deepPath));
    }

    // ── Round-trip ────────────────────────────────────────────────────────────

    [Fact]
    public void SaveThenLoad_RoundTrips()
    {
        var original = new PersistedSettings
        {
            Host = "roundtrip.nas",
            Username = "carol",
            Port = 5002m,
            UseHttps = false,
            Mountpoint = "/data",
            CacheTtl = 10m,
            ReadCacheMb = 64m,
            LogLevel = "trace",
        };

        SettingsService.Save(original, _path);
        var loaded = SettingsService.Load(_path);

        Assert.Equal(original.Host, loaded.Host);
        Assert.Equal(original.Username, loaded.Username);
        Assert.Equal(original.Port, loaded.Port);
        Assert.Equal(original.UseHttps, loaded.UseHttps);
        Assert.Equal(original.Mountpoint, loaded.Mountpoint);
        Assert.Equal(original.CacheTtl, loaded.CacheTtl);
        Assert.Equal(original.ReadCacheMb, loaded.ReadCacheMb);
        Assert.Equal(original.LogLevel, loaded.LogLevel);
    }

    // ── The SmbDomain → Domain rename ─────────────────────────────────────────

    /// <summary>
    /// The field was called "SmbDomain" while SMB was the only leg that
    /// authenticated against the directory. The VPN needs it too — DSM refuses
    /// an unqualified name before it looks at the password — so it was renamed.
    ///
    /// Dropping the existing value on that rename is not a cosmetic loss: an
    /// empty domain sends the unqualified name, DSM rejects it, and its
    /// auto-block is three strikes in 24h and never expires. The migration is
    /// cheaper than the permanent unblock chore it avoids.
    /// </summary>
    [Fact]
    public void Load_JsonWithTheOldSmbDomainKey_KeepsTheDomain()
    {
        File.WriteAllText(_path, """{"Host":"nas","SmbDomain":"KRG"}""");

        Assert.Equal("KRG", SettingsService.Load(_path).Domain);
    }

    /// <summary>A file written since the rename is the authority on its own
    /// value; the stale key beside it is not a second opinion.</summary>
    [Fact]
    public void Load_JsonWithBothKeys_PrefersTheCurrentOne()
    {
        File.WriteAllText(_path, """{"Domain":"KRG","SmbDomain":"STALE"}""");

        Assert.Equal("KRG", SettingsService.Load(_path).Domain);
    }

    /// <summary>Migrated once, not carried forever: writing the old key back
    /// would keep a second source of truth alive in every settings file.</summary>
    [Fact]
    public void SaveThenLoad_DoesNotWriteBackTheOldKey()
    {
        File.WriteAllText(_path, """{"SmbDomain":"KRG"}""");

        SettingsService.Save(SettingsService.Load(_path), _path);

        var written = File.ReadAllText(_path);
        Assert.Contains("\"Domain\": \"KRG\"", written);
        Assert.DoesNotContain("SmbDomain", written);
    }

    // ── The log file ──────────────────────────────────────────────────────────

    /// <summary>Writing a log file is opt-in, so a settings.json written before
    /// the option existed must load with it off. Turning it on for everybody at
    /// upgrade would start writing to a path nobody chose.</summary>
    [Fact]
    public void Load_JsonPredatingTheLogFile_DefaultsToOff()
    {
        File.WriteAllText(_path, """{"Host":"nas.local","LogLevel":"info"}""");

        var s = SettingsService.Load(_path);

        Assert.False(s.LogToFile);
        Assert.Null(SettingsService.ResolveLogFile(s.LogToFile, s.LogFilePath));
    }

    [Fact]
    public void ResolveLogFile_NotWanted_IsNull()
    {
        Assert.Null(SettingsService.ResolveLogFile(false, "/tmp/somewhere.log"));
    }

    /// <summary>Ticking the box is enough. The point of the feature is to have
    /// a log after a lock-up, and requiring a path first is a way of finding
    /// out afterwards that nobody set one.</summary>
    [Fact]
    public void ResolveLogFile_WantedWithNoPath_UsesTheDefaultBesideTheSettings()
    {
        Assert.Equal(
            SettingsService.DefaultLogFilePath,
            SettingsService.ResolveLogFile(true, ""));
        Assert.Equal(
            SettingsService.DefaultLogFilePath,
            SettingsService.ResolveLogFile(true, "   "));
        Assert.Equal(
            SettingsService.DefaultLogFilePath,
            SettingsService.ResolveLogFile(true, null));
    }

    [Fact]
    public void ResolveLogFile_WantedWithAPath_UsesIt()
    {
        var chosen = Path.Combine(_dir, "mount.log");

        Assert.Equal(chosen, SettingsService.ResolveLogFile(true, chosen));
    }

    /// <summary>The default sits beside the settings and the downloaded VPN
    /// profile, in the same per-user directory — somewhere writable without a
    /// prompt, and somewhere a person can be told to look.</summary>
    [Fact]
    public void DefaultLogFilePath_LivesWithTheOtherPerUserState()
    {
        Assert.Equal(
            Path.GetDirectoryName(SettingsService.DefaultVpnProfilePath),
            Path.GetDirectoryName(SettingsService.DefaultLogFilePath));
    }
}
