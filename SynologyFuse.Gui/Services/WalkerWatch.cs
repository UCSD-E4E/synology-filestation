using System;
using System.Collections.Generic;
using System.Linq;
using SynologyFuse.Gui.Models;

namespace SynologyFuse.Gui.Services;

/// <summary>What one look at the mount's walkers found: the ones to show, and
/// the ones seen for the first time.</summary>
public sealed record WalkerUpdate(IReadOnlyList<MountWalker> Shown, IReadOnlyList<MountWalker> Started);

/// <summary>
/// Re-reads which processes are walking the whole mount, and remembers which
/// ones the user has said to leave alone.
///
/// A search started from <c>/</c> or the home folder walks into the mount and
/// reads every folder on the NAS; the mount notices (<c>walks</c> in the fuse
/// crate) and this is how the window hears about it. Asked on the same tick as
/// <see cref="TransportWatch"/>: the native side only reads what it has
/// already counted.
///
/// A dismissal lasts as long as that walk: once the process is no longer
/// listed it is forgotten, since its id may be given to a different program.
/// </summary>
public sealed class WalkerWatch
{
    private readonly Func<IReadOnlyList<MountWalker>> _read;
    private readonly HashSet<uint> _seen = new();
    private readonly HashSet<uint> _dismissed = new();
    /// <summary>What the last good read said, for a read that fails.</summary>
    private IReadOnlyList<MountWalker> _last = Array.Empty<MountWalker>();

    public WalkerWatch(Func<IReadOnlyList<MountWalker>> read)
    {
        _read = read;
    }

    /// <summary>Look again. A read that throws changes nothing: this runs on
    /// the UI thread's timer, where an exception would take the window down,
    /// and the next tick asks again.</summary>
    public WalkerUpdate Poll()
    {
        IReadOnlyList<MountWalker> now;
        try
        {
            now = _read();
        }
        catch (Exception)
        {
            return new WalkerUpdate(Shown(_last), Array.Empty<MountWalker>());
        }
        var pids = now.Select(w => w.Pid).ToHashSet();
        _seen.IntersectWith(pids);
        _dismissed.IntersectWith(pids);

        var started = now.Where(w => _seen.Add(w.Pid)).ToList();
        _last = now;
        return new WalkerUpdate(Shown(now), started);
    }

    private List<MountWalker> Shown(IReadOnlyList<MountWalker> walkers) =>
        walkers.Where(w => !_dismissed.Contains(w.Pid)).ToList();

    /// <summary>Stop showing <paramref name="pid"/> for the rest of its walk.</summary>
    public void Dismiss(uint pid) => _dismissed.Add(pid);

    /// <summary>Forget everything: the mount these walkers were on is gone, and
    /// what walks the next one is news there.</summary>
    public void Reset()
    {
        _seen.Clear();
        _dismissed.Clear();
        _last = Array.Empty<MountWalker>();
    }
}
