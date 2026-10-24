using System;
using System.Globalization;
using SynologyFuse.Gui.Models;

namespace SynologyFuse.Gui.Services;

/// <summary>
/// The words for a <see cref="MountWalker"/>. A pure function, so the wording
/// is tested without a mount behind it.
/// </summary>
public static class WalkerPresenter
{
    /// <summary>What the banner says under the walkers, once for all of them.</summary>
    public const string Remedy =
        "A search started from / or your home folder walks into the mount and reads every " +
        "folder on the NAS, which takes days. Stop it unless that is what it was meant to do; " +
        "searches that skip the mount (find -xdev) never get here.";

    /// <summary>The executable's name, as the mount read it from the first
    /// argument. Not parsed out of <see cref="MountWalker.Command"/>: that is the
    /// arguments joined with spaces, so a path with a space in it is no longer
    /// one word.</summary>
    public static string Program(MountWalker walker) =>
        string.IsNullOrWhiteSpace(walker.Program) ? "A program" : walker.Program;

    public static string Duration(ulong secs) => secs switch
    {
        < 60 => $"{secs} s",
        < 3600 => $"{secs / 60} min",
        < 86400 => $"{secs / 3600} h {secs % 3600 / 60} min",
        _ => $"{secs / 86400} d {secs % 86400 / 3600} h",
    };

    public static string Headline(MountWalker walker) =>
        $"{Program(walker)} (pid {walker.Pid}) is reading every folder on the NAS";

    public static string Summary(MountWalker walker) =>
        string.Create(
            CultureInfo.InvariantCulture,
            $"{walker.DirsOpened:N0} folders in {Duration(walker.WalkingSecs)}, " +
            $"{walker.DirsOpen:N0} open now. Last in {walker.Latest}.");
}
