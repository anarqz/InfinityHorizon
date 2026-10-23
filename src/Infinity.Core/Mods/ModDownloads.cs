// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
namespace Infinity.Core.Mods;
public sealed class ModException(string message) : Exception(message);
public sealed class ModSettingsChangedException() : IOException("settings changed outside the launcher");
internal static class ModDownloads
{
    internal static async Task<HttpResponseMessage> DownloadAsync(HttpClient http, Uri location, CancellationToken cancellation)
    {
        if (http.DefaultRequestHeaders.Any())
            throw new ModException("a dedicated download client is required");
        var visited = new HashSet<Uri>();
        for (var redirects = 0; ; redirects++)
        {
            if (!location.IsAbsoluteUri || location.Scheme != Uri.UriSchemeHttps || !location.IsDefaultPort
                || location.UserInfo.Length != 0 || location.Fragment.Length != 0
                || location.Host is not ("github.com" or "release-assets.githubusercontent.com" or "objects.githubusercontent.com" or "github-releases.githubusercontent.com")
                || !visited.Add(location))
                throw new ModException("the mod download destination is not supported");
            using var request = new HttpRequestMessage(HttpMethod.Get, location);
            request.Headers.TryAddWithoutValidation("User-Agent", "infinity-launcher");
            var response = await http.SendAsync(request, HttpCompletionOption.ResponseHeadersRead, cancellation).ConfigureAwait(false);
            if (response.RequestMessage?.RequestUri is { } actual && actual != location)
            {
                response.Dispose();
                throw new ModException("automatic download redirects are not supported");
            }
            if (response.StatusCode is not (HttpStatusCode.MovedPermanently or HttpStatusCode.Found or HttpStatusCode.SeeOther
                or HttpStatusCode.TemporaryRedirect or HttpStatusCode.PermanentRedirect))
                return response;
            var next = response.Headers.Location;
            response.Dispose();
            if (redirects >= 5 || next is null || !Uri.TryCreate(location, next, out var resolved))
                throw new ModException("the mod download redirected too many times or has no destination");
            location = resolved;
        }
    }

}
