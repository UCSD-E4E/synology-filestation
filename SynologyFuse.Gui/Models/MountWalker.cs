using System.Text.Json.Serialization;

namespace SynologyFuse.Gui.Models;

/// <summary>
/// A process walking the whole mount, as <c>syno_mount_walkers</c> reports it
/// (<c>walks::Walker</c> in the fuse crate).
/// </summary>
public sealed record MountWalker
{
    [JsonPropertyName("pid")] public uint Pid { get; init; }
    [JsonPropertyName("program")] public string Program { get; init; } = "";
    [JsonPropertyName("command")] public string Command { get; init; } = "";
    [JsonPropertyName("walking_secs")] public ulong WalkingSecs { get; init; }
    [JsonPropertyName("dirs_opened")] public ulong DirsOpened { get; init; }
    [JsonPropertyName("dirs_open")] public ulong DirsOpen { get; init; }
    [JsonPropertyName("latest")] public string Latest { get; init; } = "";
}
