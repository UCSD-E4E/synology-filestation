using System;
using System.Collections.Generic;
using System.Collections.ObjectModel;
using System.Linq;
using CommunityToolkit.Mvvm.ComponentModel;
using CommunityToolkit.Mvvm.Input;
using SynologyFuse.Gui.Models;
using SynologyFuse.Gui.Services;

namespace SynologyFuse.Gui.ViewModels;

/// <summary>
/// The banner naming each process that is walking the whole NAS, with a way
/// to stop it. A search from <c>/</c> or the home folder that wanders into the
/// mount runs for days, and nothing else would ever say so: the program that
/// started it has usually stopped reading its output long before.
///
/// Holds no native handles: what is walking comes from a
/// <see cref="WalkerWatch"/>, and stopping one is a delegate, so it is tested
/// on its own like <see cref="ErrorBannerViewModel"/>.
/// </summary>
public sealed partial class WalkersBannerViewModel : ObservableObject
{
    private readonly WalkerWatch _watch;
    private readonly Action<uint> _stop;
    private readonly Action<string> _log;

    /// <summary>How many refreshes a failure stays on its row: half a minute
    /// at <see cref="TransportWatch.Interval"/>. Long enough to read, and not
    /// so long it is still there, describing an old attempt, for the rest of
    /// a walk that can last days.</summary>
    public const int ErrorRefreshes = 6;

    /// <summary>Walkers Stop was sent to, until they are gone or seen to be
    /// still walking. SIGTERM can be handled or ignored, and a row that
    /// vanished and quietly came back read as a success.</summary>
    private readonly HashSet<uint> _stopped = new();

    /// <param name="stop">Ends a walker; throws, with a message worth showing,
    /// when it cannot.</param>
    /// <param name="log">Takes a line for the log pane.</param>
    public WalkersBannerViewModel(WalkerWatch watch, Action<uint> stop, Action<string> log)
    {
        _watch = watch;
        _stop = stop;
        _log = log;
        Items.CollectionChanged += (_, _) => OnPropertyChanged(nameof(IsVisible));
    }

    public ObservableCollection<WalkerItemViewModel> Items { get; } = new();

    public bool IsVisible => Items.Count > 0;

    public string Remedy => WalkerPresenter.Remedy;

    /// <summary>Look again, logging each walker the first time it is seen.</summary>
    public void Refresh()
    {
        var update = _watch.Poll();
        foreach (var walker in update.Started)
        {
            _log($"[walk] {WalkerPresenter.Headline(walker)}: {walker.Command}");
        }

        _stopped.IntersectWith(update.Shown.Select(w => w.Pid));
        foreach (var gone in Items.Where(i => update.Shown.All(w => w.Pid != i.Pid)).ToList())
        {
            Items.Remove(gone);
        }
        foreach (var walker in update.Shown)
        {
            var item = Items.FirstOrDefault(i => i.Pid == walker.Pid);
            if (item is null)
            {
                item = new WalkerItemViewModel(walker.Pid, Stop, Ignore);
                Items.Add(item);
            }
            item.Show(walker);
            if (_stopped.Remove(walker.Pid))
            {
                item.Fail(
                    $"It is still running after Stop, so it may be ignoring the signal. " +
                    $"kill -9 {walker.Pid} ends it for certain.");
                _log($"[walk] {item.Program} (pid {walker.Pid}) is still walking after Stop.");
            }
        }
    }

    /// <summary>Empty the banner: the mount the walkers were on is gone.</summary>
    public void Clear()
    {
        Items.Clear();
        _stopped.Clear();
        _watch.Reset();
    }

    private void Stop(WalkerItemViewModel item)
    {
        item.ClearError();
        try
        {
            _stop(item.Pid);
        }
        catch (Exception e)
        {
            item.Fail(e.Message);
            return;
        }
        _stopped.Add(item.Pid);
        _log($"[walk] Stopped {item.Program} (pid {item.Pid}).");
        Items.Remove(item);
    }

    private void Ignore(WalkerItemViewModel item)
    {
        _watch.Dismiss(item.Pid);
        Items.Remove(item);
    }
}

/// <summary>One walker on the <see cref="WalkersBannerViewModel"/>.</summary>
public sealed partial class WalkerItemViewModel : ObservableObject
{
    private readonly Action<WalkerItemViewModel> _stop;
    private readonly Action<WalkerItemViewModel> _ignore;

    internal WalkerItemViewModel(uint pid, Action<WalkerItemViewModel> stop, Action<WalkerItemViewModel> ignore)
    {
        Pid = pid;
        _stop = stop;
        _ignore = ignore;
    }

    public uint Pid { get; }

    /// <summary>The program's name, for the log line when it is stopped.</summary>
    public string Program { get; private set; } = "";

    [ObservableProperty]
    private string _headline = "";

    [ObservableProperty]
    private string _summary = "";

    /// <summary>The full command line, so it is clear which search this is.</summary>
    [ObservableProperty]
    private string _command = "";

    /// <summary>Why the last Stop did not work, if it did not.</summary>
    [ObservableProperty]
    [NotifyPropertyChangedFor(nameof(HasError))]
    private string _error = "";

    public bool HasError => Error.Length > 0;

    /// <summary>Refreshes left before <see cref="Error"/> is cleared.</summary>
    private int _errorTicks;

    internal void Fail(string message)
    {
        Error = message;
        _errorTicks = WalkersBannerViewModel.ErrorRefreshes;
    }

    internal void ClearError()
    {
        Error = "";
        _errorTicks = 0;
    }

    internal void Show(MountWalker walker)
    {
        if (_errorTicks > 0 && --_errorTicks == 0)
        {
            Error = "";
        }
        Program = WalkerPresenter.Program(walker);
        Headline = WalkerPresenter.Headline(walker);
        Summary = WalkerPresenter.Summary(walker);
        Command = walker.Command;
    }

    [RelayCommand]
    private void Stop() => _stop(this);

    [RelayCommand]
    private void Ignore() => _ignore(this);
}
