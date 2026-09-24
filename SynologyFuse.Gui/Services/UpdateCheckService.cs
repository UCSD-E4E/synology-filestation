using System;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Net.Http.Json;
using System.Reflection;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Threading;
using System.Threading.Tasks;

namespace SynologyFuse.Gui.Services;

public sealed record UpdateInfo(Version Latest, string TagName, string HtmlUrl);

/// <summary>
/// Queries the GitHub Releases API for the newest published release and reports
/// whether it is newer than the running build.
/// </summary>
public static class UpdateCheckService
{
    // We list releases instead of hitting /releases/latest because until
    // 0.7.0 every version was published as four releases (GUI, FUSE crate,
    // Rust core, Python bindings), and /releases/latest returned whichever
    // was published last. From 0.8.0 there is one release per version, but
    // its tag keeps the FUSE prefix precisely so that GUIs already installed
    // — which filter on it — still see updates. Do not change the prefix.
    private const string ReleasesUrl =
        "https://api.github.com/repos/UCSD-E4E/synology-filestation/releases?per_page=20";

    private const string FuseTagPrefix = "synology-filestation-fuse-v";

    private static readonly HttpClient Http = CreateClient();

    private static HttpClient CreateClient()
    {
        var c = new HttpClient { Timeout = TimeSpan.FromSeconds(10) };
        c.DefaultRequestHeaders.UserAgent.Add(
            new ProductInfoHeaderValue("SynologyFuse-Gui", CurrentVersion().ToString()));
        c.DefaultRequestHeaders.Accept.Add(
            new MediaTypeWithQualityHeaderValue("application/vnd.github+json"));
        return c;
    }

    public static Version CurrentVersion() =>
        Assembly.GetExecutingAssembly().GetName().Version ?? new Version(0, 0, 0);

    /// <summary>
    /// Returns release info if a newer version is available, otherwise null.
    /// Network/parse errors are swallowed and return null — update checks are
    /// best-effort and must never block the UI.
    /// </summary>
    public static async Task<UpdateInfo?> CheckAsync(CancellationToken ct = default)
    {
        try
        {
            var releases = await Http.GetFromJsonSafeAsync<GithubRelease[]>(ReleasesUrl, ct);
            return EvaluateReleases(releases, CurrentVersion());
        }
        catch
        {
            return null;
        }
    }

    /// <summary>
    /// Pure version-comparison logic, factored out of <see cref="CheckAsync"/>
    /// so it can be unit-tested without HTTP. Walks a list of GitHub
    /// releases (newest first), finds the newest one whose tag starts with
    /// <c>synology-filestation-fuse-v</c>, and reports whether it is newer
    /// than <paramref name="current"/>.
    /// </summary>
    internal static UpdateInfo? EvaluateReleases(string json, Version current)
    {
        try
        {
            var releases = JsonSerializer.Deserialize<GithubRelease[]>(json);
            return EvaluateReleases(releases, current);
        }
        catch (JsonException)
        {
            return null;
        }
    }

    private static UpdateInfo? EvaluateReleases(GithubRelease[]? releases, Version current)
    {
        if (releases is null) return null;
        // GitHub returns releases sorted by created_at descending, so the
        // first FUSE-prefixed tag we encounter is the newest one.
        foreach (var release in releases)
        {
            if (release?.TagName is null) continue;
            if (!release.TagName.StartsWith(FuseTagPrefix, StringComparison.Ordinal)) continue;
            return EvaluateRelease(release, current);
        }
        return null;
    }

    /// <summary>
    /// Compare a single release against <paramref name="current"/>. Kept
    /// internal for direct unit testing of the parse+compare path; the
    /// production code path is <see cref="EvaluateReleases(string, Version)"/>
    /// (plural), which filters first and then delegates here.
    /// </summary>
    internal static UpdateInfo? EvaluateRelease(string json, Version current)
    {
        try
        {
            var release = JsonSerializer.Deserialize<GithubRelease>(json);
            return EvaluateRelease(release, current);
        }
        catch (JsonException)
        {
            return null;
        }
    }

    private static UpdateInfo? EvaluateRelease(GithubRelease? release, Version current)
    {
        if (release is null || string.IsNullOrEmpty(release.TagName))
            return null;

        if (!TryParseTag(release.TagName, out var latest))
            return null;

        return latest > current
            ? new UpdateInfo(latest, release.TagName, release.HtmlUrl ?? "")
            : null;
    }

    /// <summary>
    /// Parses release tags like "synology-filestation-fuse-v0.1.13" or "v0.1.13".
    /// </summary>
    internal static bool TryParseTag(string tag, out Version version)
    {
        version = new Version(0, 0, 0);
        var idx = tag.LastIndexOf('v');
        if (idx < 0 || idx + 1 >= tag.Length) return false;
        return Version.TryParse(tag[(idx + 1)..], out version!);
    }

    private sealed class GithubRelease
    {
        [JsonPropertyName("tag_name")]
        public string TagName { get; set; } = "";

        [JsonPropertyName("html_url")]
        public string? HtmlUrl { get; set; }
    }
}

internal static class HttpClientJsonExtensions
{
    public static async Task<T?> GetFromJsonSafeAsync<T>(
        this HttpClient http, string url, CancellationToken ct)
    {
        using var resp = await http.GetAsync(url, ct);
        if (!resp.IsSuccessStatusCode) return default;
        return await resp.Content.ReadFromJsonAsync<T>(cancellationToken: ct);
    }
}
