using System;
using System.Collections.Generic;
using System.Threading.Tasks;
using SynologyFuse.Gui.Interop;
using SynologyFuse.Gui.Models;
using SynologyFuse.Gui.Services;
using SynologyFuse.Gui.ViewModels;
using Xunit;

namespace SynologyFuse.Tests;

/// <summary>
/// Covers the browser's clipboard surface and the NAS-side MD5 action. The
/// clipboard commands never touch the native client; the MD5 tests swap the
/// native hash call for a fake through the internal constructor, so neither
/// needs a NAS session.
/// </summary>
public class FileBrowserViewModelTests
{
    private sealed class FakeClipboard : IClipboardService
    {
        public List<string> Writes { get; } = new();
        public string? Last => Writes.Count == 0 ? null : Writes[^1];
        public Task SetTextAsync(string text)
        {
            Writes.Add(text);
            return Task.CompletedTask;
        }
    }

    private static (FileBrowserViewModel Vm, FakeClipboard Clip) NewVm()
    {
        var clip = new FakeClipboard();
        return (new FileBrowserViewModel(new MountConfig(), clip), clip);
    }

    // ── Current path (the browser's path bar / window title) ───────────────────

    [Fact]
    public async Task CopyCurrentPath_CopiesTheCurrentDirectory()
    {
        var (vm, clip) = NewVm();
        vm.CurrentPath = "/photos/2026";

        await vm.CopyCurrentPathCommand.ExecuteAsync(null);

        Assert.Equal("/photos/2026", clip.Last);
    }

    [Fact]
    public void CopyCurrentPath_DisabledAtSharesRoot()
    {
        var (vm, _) = NewVm();
        vm.CurrentPath = "";

        Assert.False(vm.CopyCurrentPathCommand.CanExecute(null));
    }

    [Fact]
    public void CopyCurrentPath_ReenabledWhenPathChanges()
    {
        var (vm, _) = NewVm();
        vm.CurrentPath = "";
        Assert.False(vm.CopyCurrentPathCommand.CanExecute(null));

        vm.CurrentPath = "/photos";

        Assert.True(vm.CopyCurrentPathCommand.CanExecute(null));
    }

    [Fact]
    public async Task CopyCurrentPath_NoClipboard_DoesNotThrow()
    {
        var vm = new FileBrowserViewModel(new MountConfig()) { CurrentPath = "/photos" };

        await vm.CopyCurrentPathCommand.ExecuteAsync(null);
    }

    // ── Selected item (right-click on a file or folder) ────────────────────────

    [Fact]
    public async Task CopySelectedPath_CopiesFullPathOfSelection()
    {
        var (vm, clip) = NewVm();
        vm.CurrentPath = "/photos";
        vm.SelectedItem = new SynoFileInfo { Name = "raw.ORF", Path = "/photos/raw.ORF" };

        await vm.CopySelectedPathCommand.ExecuteAsync(null);

        Assert.Equal("/photos/raw.ORF", clip.Last);
    }

    [Fact]
    public async Task CopySelectedPath_WorksForDirectories()
    {
        var (vm, clip) = NewVm();
        vm.SelectedItem = new SynoFileInfo { Name = "2026", Path = "/photos/2026", IsDir = true };

        await vm.CopySelectedPathCommand.ExecuteAsync(null);

        Assert.Equal("/photos/2026", clip.Last);
    }

    [Fact]
    public async Task CopySelectedName_CopiesJustTheName()
    {
        var (vm, clip) = NewVm();
        vm.SelectedItem = new SynoFileInfo { Name = "raw.ORF", Path = "/photos/raw.ORF" };

        await vm.CopySelectedNameCommand.ExecuteAsync(null);

        Assert.Equal("raw.ORF", clip.Last);
    }

    [Fact]
    public void CopySelected_DisabledWithNoSelection()
    {
        var (vm, _) = NewVm();

        Assert.False(vm.CopySelectedPathCommand.CanExecute(null));
        Assert.False(vm.CopySelectedNameCommand.CanExecute(null));
    }

    [Fact]
    public void CopySelected_EnabledOnceAnItemIsSelected()
    {
        var (vm, _) = NewVm();

        vm.SelectedItem = new SynoFileInfo { Name = "raw.ORF", Path = "/photos/raw.ORF" };

        Assert.True(vm.CopySelectedPathCommand.CanExecute(null));
        Assert.True(vm.CopySelectedNameCommand.CanExecute(null));
    }

    // ── Feedback ──────────────────────────────────────────────────────────────

    [Fact]
    public async Task Copy_ReportsWhatWasCopiedInTheStatusLine()
    {
        var (vm, _) = NewVm();
        vm.SelectedItem = new SynoFileInfo { Name = "raw.ORF", Path = "/photos/raw.ORF" };

        await vm.CopySelectedPathCommand.ExecuteAsync(null);

        Assert.Contains("/photos/raw.ORF", vm.Status);
    }

    // ── Window title ──────────────────────────────────────────────────────────

    [Fact]
    public void Title_ShowsSharesRootWhenNoPath()
    {
        var (vm, _) = NewVm();
        vm.CurrentPath = "";

        Assert.Equal("Browse NAS", vm.Title);
    }

    [Fact]
    public void Title_TracksCurrentPath()
    {
        var (vm, _) = NewVm();

        vm.CurrentPath = "/photos/2026";

        Assert.Equal("Browse NAS — /photos/2026", vm.Title);
    }

    // ── MD5 (DSM hashes the file; can take minutes) ─────────────────────────

    private static readonly SynoFileInfo AFile =
        new() { Name = "raw.ORF", Path = "/photos/raw.ORF" };

    private static (FileBrowserViewModel Vm, FakeClipboard Clip, List<string> Asked) NewHashingVm(
        Func<string, Task<string?>> hash)
    {
        var clip = new FakeClipboard();
        var asked = new List<string>();
        var vm = new FileBrowserViewModel(new MountConfig(), clip, path =>
        {
            asked.Add(path);
            return hash(path);
        });
        return (vm, clip, asked);
    }

    [Fact]
    public void ComputeMd5_DisabledWithNoSelection()
    {
        var (vm, _, _) = NewHashingVm(_ => Task.FromResult<string?>("x"));

        Assert.False(vm.ComputeMd5Command.CanExecute(null));
    }

    [Fact]
    public void ComputeMd5_DisabledForFolders()
    {
        // DSM's MD5 task hashes a file; a folder has nothing to hash.
        var (vm, _, _) = NewHashingVm(_ => Task.FromResult<string?>("x"));

        vm.SelectedItem = new SynoFileInfo { Name = "2026", Path = "/photos/2026", IsDir = true };

        Assert.False(vm.ComputeMd5Command.CanExecute(null));
    }

    [Fact]
    public void ComputeMd5_EnabledOnceAFileIsSelected()
    {
        var (vm, _, _) = NewHashingVm(_ => Task.FromResult<string?>("x"));

        vm.SelectedItem = AFile;

        Assert.True(vm.ComputeMd5Command.CanExecute(null));
    }

    [Fact]
    public async Task ComputeMd5_HashesTheSelectedPath_AndShowsTheDigest()
    {
        var (vm, _, asked) = NewHashingVm(_ => Task.FromResult<string?>("9e107d9d372bb6826bd81d3542a419d6"));
        vm.SelectedItem = AFile;

        await vm.ComputeMd5Command.ExecuteAsync(null);

        Assert.Equal(new[] { "/photos/raw.ORF" }, asked);
        Assert.Equal("9e107d9d372bb6826bd81d3542a419d6", vm.Md5Result);
        Assert.Equal("raw.ORF", vm.Md5FileName);
        Assert.True(vm.HasMd5Result);
        Assert.False(vm.IsBusy);
        Assert.False(vm.ShowProgress);
    }

    [Fact]
    public async Task CopyMd5_CopiesTheDigest()
    {
        var (vm, clip, _) = NewHashingVm(_ => Task.FromResult<string?>("9e107d9d372bb6826bd81d3542a419d6"));
        vm.SelectedItem = AFile;
        Assert.False(vm.CopyMd5Command.CanExecute(null));

        await vm.ComputeMd5Command.ExecuteAsync(null);
        Assert.True(vm.CopyMd5Command.CanExecute(null));
        await vm.CopyMd5Command.ExecuteAsync(null);

        Assert.Equal("9e107d9d372bb6826bd81d3542a419d6", clip.Last);
    }

    [Fact]
    public async Task ComputeMd5_ShowsItIsWorkingUntilTheNasAnswers()
    {
        var answer = new TaskCompletionSource<string?>();
        var (vm, _, _) = NewHashingVm(_ => answer.Task);
        vm.SelectedItem = AFile;

        var running = vm.ComputeMd5Command.ExecuteAsync(null);

        Assert.True(vm.IsBusy);
        Assert.True(vm.ShowProgress);
        Assert.True(vm.ProgressIndeterminate, "DSM reports no progress, so the bar must not pretend to");
        Assert.Contains("raw.ORF", vm.Status);
        // A second hash is not queued behind the first.
        Assert.False(vm.ComputeMd5Command.CanExecute(null));

        answer.SetResult("9e107d9d372bb6826bd81d3542a419d6");
        await running;

        Assert.False(vm.IsBusy);
        Assert.False(vm.ShowProgress);
        Assert.True(vm.ComputeMd5Command.CanExecute(null));
    }

    [Fact]
    public async Task ComputeMd5_ClearsAnEarlierDigestWhenItStarts()
    {
        var answer = new TaskCompletionSource<string?>();
        var calls = 0;
        var (vm, _, _) = NewHashingVm(_ => ++calls == 1
            ? Task.FromResult<string?>("9e107d9d372bb6826bd81d3542a419d6")
            : answer.Task);
        vm.SelectedItem = AFile;
        await vm.ComputeMd5Command.ExecuteAsync(null);

        vm.SelectedItem = new SynoFileInfo { Name = "other.ORF", Path = "/photos/other.ORF" };
        var running = vm.ComputeMd5Command.ExecuteAsync(null);

        // Otherwise the old file's digest sits beside a new file's progress.
        Assert.False(vm.HasMd5Result);
        answer.SetResult("d41d8cd98f00b204e9800998ecf8427e");
        await running;
        Assert.Equal("other.ORF", vm.Md5FileName);
    }

    [Fact]
    public async Task ComputeMd5_AFailureIsExplainedInTheStatusLine()
    {
        var refused = new SynoException(NativeMethods.SynoStatus.PermissionDenied, 408, "Synology API error 408");
        var (vm, _, _) = NewHashingVm(_ => Task.FromException<string?>(refused));
        vm.SelectedItem = AFile;

        await vm.ComputeMd5Command.ExecuteAsync(null);

        Assert.Contains("raw.ORF", vm.Status);
        Assert.Contains(ErrorPresenter.Describe(refused).Title, vm.Status);
        Assert.False(vm.HasMd5Result);
        Assert.False(vm.IsBusy);
        Assert.False(vm.ShowProgress);
    }

    // The browser has one progress bar, and a transfer and a hash used to share
    // it unawares: a hash started mid-download turned the download's byte count
    // into an endless sweep, and whichever finished first hid the bar on the
    // other. Every native call queues on one gate anyway, so letting them
    // overlap bought nothing but the confusion.

    [Fact]
    public void ComputeMd5_UnavailableWhileTheProgressBarIsInUse()
    {
        var (vm, _, _) = NewHashingVm(_ => Task.FromResult<string?>("x"));
        vm.SelectedItem = AFile;
        Assert.True(vm.ComputeMd5Command.CanExecute(null));

        vm.ShowProgress = true; // a download, upload, delete or mkdir running

        Assert.False(vm.ComputeMd5Command.CanExecute(null));

        vm.ShowProgress = false;

        Assert.True(vm.ComputeMd5Command.CanExecute(null));
    }

    [Fact]
    public async Task ADownloadStartedDuringAHash_IsRefusedAndLeavesTheHashBarAlone()
    {
        var answer = new TaskCompletionSource<string?>();
        var (vm, _, _) = NewHashingVm(_ => answer.Task);
        vm.SelectedItem = AFile;
        var running = vm.ComputeMd5Command.ExecuteAsync(null);

        await vm.DownloadAsync(AFile, "/tmp/raw.ORF");

        Assert.StartsWith("Wait for the MD5 of raw.ORF", vm.Status);
        Assert.True(vm.ShowProgress, "the hash still has its bar");
        Assert.True(vm.ProgressIndeterminate);

        answer.SetResult("9e107d9d372bb6826bd81d3542a419d6");
        await running;
        Assert.Equal("9e107d9d372bb6826bd81d3542a419d6", vm.Md5Result);
    }
}
