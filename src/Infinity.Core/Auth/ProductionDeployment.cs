// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Matchmaking;
using Infinity.Core.Session;

namespace Infinity.Core.Auth;

/// <summary>Explicit production configuration. Never populated by discovery or development environment variables.</summary>
public static class ProductionDeployment
{
    public static Uri Core { get; } = new("https://api.infinity-horizon.com");
    public static Uri Gate { get; } = new("https://gate.infinity-horizon.com");
    public static Uri Saves { get; } = new("https://saves.infinity-horizon.com");
    public static Uri Matchmaking { get; } = new("https://mm.infinity-horizon.com");
    public static Uri Crashes { get; } = new("https://crashes.infinity-horizon.com");
    public const string LaunchKid = "lt-20261021-1";
    public const string LaunchPublicKey = "XQjN8vrGqvQCcrMzQU+vlHhbzF2ppID8CWET8P3lubg=";
    public const string RelayKid = "rg-20261021-1";
    public const string RelayPublicKey = "oee+y665E9oUviXkaLadB1gXfYBEreMRSmHhdRaYVkE=";

    public static bool IsOfficial(Uri uri) => new[] { Core, Gate, Saves, Matchmaking, Crashes }
        .Any(origin => string.Equals(origin.IdnHost, uri.IdnHost.TrimEnd('.'), StringComparison.OrdinalIgnoreCase));

    public static void RequireOrigin(Uri actual, Uri expected)
    {
        if (!actual.IsAbsoluteUri || actual.Scheme != Uri.UriSchemeHttps || !actual.IsDefaultPort ||
            actual.UserInfo.Length != 0 || actual.AbsolutePath != "/" || actual.Query.Length != 0 ||
            actual.Fragment.Length != 0 || !string.Equals(actual.Host, expected.Host, StringComparison.OrdinalIgnoreCase))
            throw new InvalidDataException("Production service origin is not approved");
    }

    public static void ValidateInfo(ServerInfo info)
    {
        if (info.ApiVersion != 1 || info.Realm != "forzainfinity" || info.Idp is not { ProjectId: "forza-infinity-online" } idp ||
            string.IsNullOrWhiteSpace(idp.ApiKey) || idp.EmulatorUrl is not null || idp.Kind is not (null or IdpKinds.Firebase) ||
            info.Services is not { } services)
            throw new InvalidDataException("Production discovery is invalid");
        void Check(string value, Uri expected)
        {
            if (!Uri.TryCreate(value, UriKind.Absolute, out var origin))
                throw new InvalidDataException("Production discovery origin is invalid");
            RequireOrigin(origin, expected);
        }
        Check(services.Core, Core);
        Check(services.Gate, Gate);
        Check(services.Saves, Saves);
        Check(services.Matchmaking, Matchmaking);
    }

    public static void ValidateDevelopmentInfo(ServerInfo info)
    {
        if (info.Idp?.ProjectId == "forza-infinity-online")
            throw new InvalidDataException("Production identity cannot be selected by development discovery");
        var services = info.Services;
        foreach (var value in new[] { services?.Core, services?.Gate, services?.Saves, services?.Matchmaking, info.Idp?.EmulatorUrl })
            if (Uri.TryCreate(value, UriKind.Absolute, out var origin) && IsOfficial(origin))
                throw new InvalidDataException("Production origins cannot be selected by development discovery");
    }

    public static TicketKeys LaunchKeys() => new((LaunchKid, Convert.FromBase64String(LaunchPublicKey)));
    public static RelayGrantVerifyKeys RelayKeys() => new((RelayKid, Convert.FromBase64String(RelayPublicKey)));
}
