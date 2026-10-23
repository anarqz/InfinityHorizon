// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
namespace Infinity.Core.Mods;

/// <summary>
/// A refused mod operation. <see cref="Code"/> is a stable, mod-agnostic category the launcher maps
/// to a localized message (mods.error.&lt;code&gt;); the message itself is for the log only.
/// </summary>
public sealed class ModException(string message, string code = ModErrors.Failed) : Exception(message)
{
    public string Code { get; } = code;
}

/// <summary>The stable categories of <see cref="ModException.Code"/>.</summary>
public static class ModErrors
{
    public const string Failed = "failed";
    public const string GameRunning = "game_running";
    public const string Drift = "drift";
    public const string Dependency = "dependency";
    public const string Conflict = "conflict";
    public const string Recovery = "recovery";
    public const string Linked = "linked";
    public const string Protected = "protected";
    public const string Digest = "digest";
    public const string Download = "download";
    public const string Unsupported = "unsupported";
    public const string Busy = "busy";
    public const string Locked = "locked";
    public const string Catalog = "catalog";
    public const string Settings = "settings";
    public const string Relink = "relink";
}

public sealed class ModSettingsChangedException() : IOException("settings changed outside the launcher");

internal static class ModDownloads
{
    /// <summary>
    /// Follows at most five redirects, validating each hop before it is requested: HTTPS on the default
    /// port, no credentials or fragment, and a host from <paramref name="hosts"/> (the signed catalog's
    /// download hosts, or the catalog transport's own bootstrap set). No host is built in.
    /// </summary>
    internal static async Task<HttpResponseMessage> DownloadAsync(HttpClient http, Uri location, IReadOnlyCollection<string> hosts, CancellationToken cancellation)
    {
        if (http.DefaultRequestHeaders.Any())
            throw new ModException("a dedicated download client is required", ModErrors.Download);
        var visited = new HashSet<Uri>();
        for (var redirects = 0; ; redirects++)
        {
            if (!location.IsAbsoluteUri || location.Scheme != Uri.UriSchemeHttps || !location.IsDefaultPort
                || location.UserInfo.Length != 0 || location.Fragment.Length != 0
                || !hosts.Contains(location.IdnHost, StringComparer.OrdinalIgnoreCase)
                || !visited.Add(location))
                throw new ModException("the mod download destination is not supported", ModErrors.Download);
            using var request = new HttpRequestMessage(HttpMethod.Get, location);
            request.Headers.TryAddWithoutValidation("User-Agent", "infinity-launcher");
            var response = await http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, cancellation).ConfigureAwait(false);
            if (response.RequestMessage?.RequestUri is { } actual && actual != location)
            {
                response.Dispose();
                throw new ModException("automatic download redirects are not supported", ModErrors.Download);
            }
            if (response.StatusCode is not (HttpStatusCode.MovedPermanently or HttpStatusCode.Found or HttpStatusCode.SeeOther
                or HttpStatusCode.TemporaryRedirect or HttpStatusCode.PermanentRedirect))
                return response;
            var next = response.Headers.Location;
            response.Dispose();
            if (redirects >= 5 || next is null || !Uri.TryCreate(location, next, out var resolved))
                throw new ModException("the mod download redirected too many times or has no destination", ModErrors.Download);
            location = resolved;
        }
    }

    /// <summary>A validated host list: lowercase DNS names only, at most 16.</summary>
    internal static IReadOnlyList<string> Hosts(System.Text.Json.JsonElement? list)
    {
        if (list is not { } value) return [];
        var hosts = value.EnumerateArray().Select(h => h.GetString() ?? "").ToArray();
        if (hosts.Length > 16 || hosts.Any(h => h.Length is < 1 or > 253 || Uri.CheckHostName(h) != UriHostNameType.Dns
            || h.Any(c => !(c is >= 'a' and <= 'z' or >= '0' and <= '9' or '.' or '-'))) || hosts.Distinct(StringComparer.Ordinal).Count() != hosts.Length)
            throw new ModException("invalid catalog download hosts", ModErrors.Catalog);
        return hosts;
    }
}
