using System.Collections.Generic;
using System.Linq;
using System.Text.Json;
using SynologyFuse.Gui.Models;
using SynologyFuse.Gui.Services;
using Xunit;

namespace SynologyFuse.Tests;

/// <summary>
/// A search started from <c>/</c> or the home folder walks into the mount and
/// reads every folder on the NAS, for days, long after whoever started it has
/// stopped reading its output. The mount notices (<c>walks</c> in the fuse
/// crate); these pin down what the window does with what it is told.
/// </summary>
public class WalkerWatchTests
{
    private static MountWalker Walker(uint pid, ulong opened = 6000) => new()
    {
        Pid = pid,
        Program = "bfs",
        Command = $"bfs / -name x # {pid}",
        WalkingSecs = 700,
        DirsOpened = opened,
        DirsOpen = 1300,
        Latest = "/home/u/mnt/share/d",
    };

    [Fact]
    public void The_native_shape_deserializes()
    {
        const string json = """
            [{"pid":42,"program":"bfs","command":"bfs / -name x","walking_secs":700,
              "dirs_opened":6000,"dirs_open":1300,"latest":"/home/u/mnt/share/d"}]
            """;

        var walkers = JsonSerializer.Deserialize<MountWalker[]>(json)!;

        Assert.Equal(Walker(42) with { Command = "bfs / -name x" }, walkers.Single());
    }

    [Fact]
    public void A_new_walker_is_shown_and_announced_once()
    {
        var live = new List<MountWalker>();
        var watch = new WalkerWatch(() => live);

        Assert.Empty(watch.Poll().Shown);

        live.Add(Walker(42));
        var first = watch.Poll();
        Assert.Equal(new uint[] { 42 }, first.Shown.Select(w => w.Pid));
        Assert.Equal(new uint[] { 42 }, first.Started.Select(w => w.Pid));

        live[0] = Walker(42, opened: 7000);
        var second = watch.Poll();
        Assert.Equal(7000UL, second.Shown.Single().DirsOpened);
        Assert.Empty(second.Started);
    }

    [Fact]
    public void A_walker_that_stops_is_no_longer_shown()
    {
        var live = new List<MountWalker> { Walker(42) };
        var watch = new WalkerWatch(() => live);
        watch.Poll();

        live.Clear();

        Assert.Empty(watch.Poll().Shown);
    }

    [Fact]
    public void A_dismissed_walker_stays_hidden_while_it_walks()
    {
        var live = new List<MountWalker> { Walker(42), Walker(43) };
        var watch = new WalkerWatch(() => live);
        watch.Poll();

        watch.Dismiss(42);

        var update = watch.Poll();
        Assert.Equal(new uint[] { 43 }, update.Shown.Select(w => w.Pid));
        Assert.Empty(update.Started);
    }

    [Fact]
    public void A_read_that_fails_changes_nothing()
    {
        // The tick runs on the UI thread; a bad answer from the native side
        // (a field it renamed, say) must not take the window down with it, nor
        // wipe what the user dismissed.
        var live = new List<MountWalker> { Walker(42), Walker(43) };
        var fail = false;
        var watch = new WalkerWatch(() => fail ? throw new JsonException("bad shape") : live);
        watch.Poll();
        watch.Dismiss(42);

        fail = true;
        var failed = watch.Poll();
        Assert.Equal(new uint[] { 43 }, failed.Shown.Select(w => w.Pid));
        Assert.Empty(failed.Started);

        fail = false;
        var next = watch.Poll();
        Assert.Equal(new uint[] { 43 }, next.Shown.Select(w => w.Pid));
        Assert.Empty(next.Started);
    }

    [Fact]
    public void Reset_forgets_what_was_seen_and_dismissed()
    {
        // On disconnect. The next mount is a new volume: a process walking it
        // is news there, even one that was ignored on the last.
        var live = new List<MountWalker> { Walker(42) };
        var watch = new WalkerWatch(() => live);
        watch.Poll();
        watch.Dismiss(42);

        watch.Reset();

        var update = watch.Poll();
        Assert.Equal(new uint[] { 42 }, update.Shown.Select(w => w.Pid));
        Assert.Equal(new uint[] { 42 }, update.Started.Select(w => w.Pid));
    }

    [Fact]
    public void A_dismissal_lasts_only_as_long_as_the_walk()
    {
        // The id is the process's, and ids are reused: a later walk under the
        // same one is a different program, and must not inherit the dismissal.
        var live = new List<MountWalker> { Walker(42) };
        var watch = new WalkerWatch(() => live);
        watch.Poll();
        watch.Dismiss(42);
        live.Clear();
        watch.Poll();

        live.Add(Walker(42));

        var update = watch.Poll();
        Assert.Equal(new uint[] { 42 }, update.Shown.Select(w => w.Pid));
        Assert.Equal(new uint[] { 42 }, update.Started.Select(w => w.Pid));
    }
}

public class WalkerPresenterTests
{
    [Theory]
    // The native side names the program from its first argument alone, so a
    // path with a space in it is still one name; the joined command line
    // cannot tell.
    [InlineData("bfs", "/home/u/My Tools/bfs / -name x", "bfs")]
    [InlineData("[kworker/0:1]", "[kworker/0:1]", "[kworker/0:1]")]
    [InlineData("", "", "A program")]
    public void The_program_is_the_name_the_mount_gave(string program, string command, string shown)
    {
        var walker = new MountWalker { Program = program, Command = command };
        Assert.Equal(shown, WalkerPresenter.Program(walker));
    }

    [Theory]
    [InlineData(45UL, "45 s")]
    [InlineData(600UL, "10 min")]
    [InlineData(7500UL, "2 h 5 min")]
    [InlineData(5UL * 86400 + 4 * 3600 + 59, "5 d 4 h")]
    public void A_duration_reads_as_a_person_would_say_it(ulong secs, string spoken)
    {
        Assert.Equal(spoken, WalkerPresenter.Duration(secs));
    }

    [Fact]
    public void The_headline_says_who_and_the_summary_says_how_much_and_where()
    {
        var walker = new MountWalker
        {
            Pid = 42,
            Program = "bfs",
            Command = "bfs / -name x",
            WalkingSecs = 7500,
            DirsOpened = 123456,
            DirsOpen = 1300,
            Latest = "/home/u/mnt/share/d",
        };

        Assert.Equal("bfs (pid 42) is reading every folder on the NAS", WalkerPresenter.Headline(walker));
        Assert.Equal(
            "123,456 folders in 2 h 5 min, 1,300 open now. Last in /home/u/mnt/share/d.",
            WalkerPresenter.Summary(walker));
    }
}
