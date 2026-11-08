// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Auth;

/// <summary>
/// The official service origins and their rules (<c>ProductionDeployment</c> exposes the same values). Never populated by
/// discovery or development environment variables. Here so the updater checks its restart arguments without the rest of Core.
/// </summary>
public static class ProductionOrigins
{
    public static Uri Core { get; } = new("https://api.infinity-horizon.com");
    public static Uri Gate { get; } = new("https://gate.infinity-horizon.com");
    public static Uri Saves { get; } = new("https://saves.infinity-horizon.com");
    public static Uri Matchmaking { get; } = new("https://mm.infinity-horizon.com");
    public static Uri Crashes { get; } = new("https://crashes.infinity-horizon.com");

    public static bool IsOfficial(Uri uri) => new[] { Core, Gate, Saves, Matchmaking, Crashes }
        .Any(origin => string.Equals(origin.IdnHost, uri.IdnHost.TrimEnd('.'), StringComparison.OrdinalIgnoreCase));

    public static void RequireOrigin(Uri actual, Uri expected)
    {
        if (!actual.IsAbsoluteUri || actual.Scheme != Uri.UriSchemeHttps || !actual.IsDefaultPort ||
            actual.UserInfo.Length != 0 || actual.AbsolutePath != "/" || actual.Query.Length != 0 ||
            actual.Fragment.Length != 0 || !string.Equals(actual.Host, expected.Host, StringComparison.OrdinalIgnoreCase))
            throw new InvalidDataException("Production service origin is not approved");
    }
}
