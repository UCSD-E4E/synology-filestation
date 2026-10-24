using System;
using System.Collections.Generic;
using System.Linq;
using SynologyFuse.Gui.Models;
using SynologyFuse.Gui.Services;
using SynologyFuse.Gui.ViewModels;
using Xunit;

namespace SynologyFuse.Tests;

/// <summary>
/// The banner that names a process walking the whole NAS and offers to stop
/// it. Driven here without a mount: the walkers are a list, stopping one is a
/// delegate, and the log is a list of lines.
/// </summary>
public class WalkersBannerViewModelTests
{
    private static MountWalker Walker(uint pid, ulong opened = 6000) => new()
    {
        Pid = pid,
        Program = "bfs",
        Command = $"bfs / -name x{pid}",
        WalkingSecs = 700,
        DirsOpened = opened,
        DirsOpen = 1300,
        Latest = "/home/u/mnt/share/d",
    };

    private sealed class Harness
    {
        public readonly List<MountWalker> Live = new();
        public readonly List<uint> Stopped = new();
        public readonly List<string> Log = new();
        public Func<uint, Exception?> StopFails = _ => null;
        public readonly WalkersBannerViewModel Banner;

        public Harness()
        {
            Banner = new WalkersBannerViewModel(
                new WalkerWatch(() => Live),
                pid =>
                {
                    if (StopFails(pid) is { } e) throw e;
                    Stopped.Add(pid);
                },
                Log.Add);
        }
    }

    [Fact]
    public void Nothing_walking_shows_nothing()
    {
        var h = new Harness();
        h.Banner.Refresh();
        Assert.False(h.Banner.IsVisible);
        Assert.Empty(h.Banner.Items);
    }

    [Fact]
    public void A_walker_is_shown_and_logged_once()
    {
        var h = new Harness();
        h.Live.Add(Walker(42));

        h.Banner.Refresh();
        h.Live[0] = Walker(42, opened: 9000);
        h.Banner.Refresh();

        Assert.True(h.Banner.IsVisible);
        var item = Assert.Single(h.Banner.Items);
        Assert.Equal(42U, item.Pid);
        Assert.Equal("bfs (pid 42) is reading every folder on the NAS", item.Headline);
        Assert.StartsWith("9,000 folders", item.Summary);
        Assert.Equal("bfs / -name x42", item.Command);
        var line = Assert.Single(h.Log);
        Assert.Contains("pid 42", line);
        Assert.Contains("bfs / -name x42", line);
    }

    [Fact]
    public void A_walker_that_ends_leaves_the_banner()
    {
        var h = new Harness();
        h.Live.Add(Walker(42));
        h.Banner.Refresh();

        h.Live.Clear();
        h.Banner.Refresh();

        Assert.False(h.Banner.IsVisible);
        Assert.Empty(h.Banner.Items);
    }

    [Fact]
    public void Stop_stops_that_process_and_takes_it_off_the_banner()
    {
        var h = new Harness();
        h.Live.AddRange(new[] { Walker(42), Walker(43) });
        h.Banner.Refresh();

        h.Banner.Items.Single(i => i.Pid == 42).StopCommand.Execute(null);

        Assert.Equal(new uint[] { 42 }, h.Stopped);
        Assert.Equal(new uint[] { 43 }, h.Banner.Items.Select(i => i.Pid));
        Assert.Contains(h.Log, l => l.Contains("Stopped") && l.Contains("pid 42"));
    }

    [Fact]
    public void A_stop_that_fails_says_why_and_keeps_it_shown()
    {
        var h = new Harness { StopFails = _ => new InvalidOperationException("that process has already exited") };
        h.Live.Add(Walker(42));
        h.Banner.Refresh();

        var item = h.Banner.Items.Single();
        item.StopCommand.Execute(null);

        Assert.Same(item, h.Banner.Items.Single());
        Assert.Equal("that process has already exited", item.Error);
        Assert.True(item.HasError);
    }

    [Fact]
    public void A_stop_that_does_not_end_it_says_so()
    {
        // SIGTERM can be handled, or wait behind a SIGSTOP. A row that vanished
        // and came back five seconds later, with nothing said, read as success.
        var h = new Harness();
        h.Live.Add(Walker(42));
        h.Banner.Refresh();
        h.Banner.Items.Single().StopCommand.Execute(null);
        Assert.Empty(h.Banner.Items);

        h.Banner.Refresh();

        var item = Assert.Single(h.Banner.Items);
        Assert.Contains("still", item.Error);
        Assert.Contains("kill -9 42", item.Error);
        Assert.Single(h.Log, l => l.Contains("still walking after Stop"));

        h.Banner.Refresh();
        Assert.Single(h.Log, l => l.Contains("still walking after Stop"));
    }

    [Fact]
    public void A_stop_error_fades_and_a_retry_clears_it()
    {
        var fails = true;
        var h = new Harness { StopFails = _ => fails ? new InvalidOperationException("boom") : null };
        h.Live.Add(Walker(42));
        h.Banner.Refresh();
        var item = h.Banner.Items.Single();

        item.StopCommand.Execute(null);
        Assert.Equal("boom", item.Error);
        for (var i = 0; i < WalkersBannerViewModel.ErrorRefreshes; i++)
        {
            Assert.True(item.HasError, $"still shown after {i} refreshes");
            h.Banner.Refresh();
        }
        Assert.False(item.HasError);

        item.StopCommand.Execute(null);
        Assert.True(item.HasError);
        fails = false;
        item.StopCommand.Execute(null);
        Assert.Empty(h.Banner.Items);
        Assert.False(item.HasError, "a retry clears the last failure");
    }

    [Fact]
    public void Ignore_hides_it_for_the_rest_of_its_walk()
    {
        var h = new Harness();
        h.Live.Add(Walker(42));
        h.Banner.Refresh();

        h.Banner.Items.Single().IgnoreCommand.Execute(null);
        Assert.Empty(h.Banner.Items);

        h.Banner.Refresh();
        Assert.Empty(h.Banner.Items);
        Assert.Empty(h.Stopped);
    }

    [Fact]
    public void Clear_empties_the_banner()
    {
        // Called on disconnect: the mount the walkers belonged to is gone.
        var h = new Harness();
        h.Live.Add(Walker(42));
        h.Banner.Refresh();

        h.Banner.Clear();

        Assert.False(h.Banner.IsVisible);
        Assert.Empty(h.Banner.Items);
    }

    [Fact]
    public void After_a_clear_the_next_mount_starts_fresh()
    {
        var h = new Harness();
        h.Live.Add(Walker(42));
        h.Banner.Refresh();
        h.Banner.Items.Single().IgnoreCommand.Execute(null);

        h.Banner.Clear();
        h.Banner.Refresh();

        Assert.Equal(42U, Assert.Single(h.Banner.Items).Pid);
        Assert.Equal(2, h.Log.Count);
    }
}
