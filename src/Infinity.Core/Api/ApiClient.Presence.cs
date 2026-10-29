// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;

namespace Infinity.Core.Api;

// CR-S72 (OD-136), CR-S77/CR-L44 (OD-137): ACCOUNTS.md §5 "Presence", §5a. Mirrors of fi_protocol::accounts::{ProfilePresence,
// PresenceClub} / packages/contracts/src/accounts.ts (ContractParityTests).

/// <summary>A club in <see cref="ProfilePresence"/>: <see cref="Tag"/> is 2 to 4 of <c>A-Z0-9</c>, <see cref="Name"/> 1 to 24 characters.</summary>
public sealed record PresenceClub(string Tag, string Name);

/// <summary>
/// <c>GET /launcher/v1/profiles/{xuid}/presence</c> → 200: the profile's Car Club of its current mode, and its level and credits from the
/// last accepted <c>UpdateAll</c> of that mode (<see cref="SyncedAt"/> its time, RFC 3339). Each field is absent when unknown
/// (<c>{}</c> before any club or accepted sync). <see cref="LiveryImage"/> (CR-S77): with <c>?car=</c>, the absolute https URL of the
/// caller's own newest servable Storefront livery preview of that car (§5a); <see cref="ApiClient.ProfilePresenceAsync"/> keeps it only
/// when it is https, without userinfo and on the core's host.
/// </summary>
public sealed record ProfilePresence(PresenceClub? Club = null, uint? Level = null, ulong? Credits = null, string? SyncedAt = null,
    string? LiveryImage = null);

public sealed partial class ApiClient
{
    /// <summary>Discord's limit on an asset key or URL.</summary>
    public const int MaxLiveryImageLength = 256;

    /// <summary>
    /// CR-S72 with the access token, the caller's own profile only. Read at game start, every 5 minutes and on a car change (CR-L44,
    /// rate-limited by <see cref="Presence.PresenceReads"/>): past 60 reads per account per hour it is 429 <c>REQUEST.RATE_LIMITED</c>
    /// with <c>Retry-After</c> (thrown; the caller honours it). <paramref name="carId"/> (<c>Data_Car.Id</c>, 1 to 2147483647) is sent as
    /// <c>?car=</c> only when known. Null on 404 (<c>ACCOUNTS.NOT_FOUND</c>: not this account's profile, or a server without the route)
    /// and 405: no club, and level and credits only from the hook.
    /// </summary>
    public async Task<ProfilePresence?> ProfilePresenceAsync(string access, string xuid, int? carId = null, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(xuid);
        var path = $"/launcher/v1/profiles/{Uri.EscapeDataString(xuid)}/presence";
        if (carId is { } car and > 0) path += "?car=" + car.ToString(CultureInfo.InvariantCulture);
        try
        {
            var presence = await SendAsync<ProfilePresence>(HttpMethod.Get, Core, path, null, access,
                idempotent: false, cancellation).ConfigureAwait(false);
            return presence is { LiveryImage: { } image } && !IsLiveryImage(image, Core) ? presence with { LiveryImage = null } : presence;
        }
        catch (ApiException unsupported) when (unsupported.Status is HttpStatusCode.NotFound or HttpStatusCode.MethodNotAllowed)
        {
            return null;
        }
    }

    /// <summary>
    /// CR-L44: <paramref name="url"/> is usable as Discord's large image: an absolute <c>https</c> URL without userinfo, on
    /// <paramref name="core"/>'s host (§5a serves previews on the core origin), at most <see cref="MaxLiveryImageLength"/> characters.
    /// </summary>
    public static bool IsLiveryImage(string? url, Uri core)
    {
        ArgumentNullException.ThrowIfNull(core);
        if (string.IsNullOrEmpty(url) || url.Length > MaxLiveryImageLength || url.Any(c => char.IsControl(c) || char.IsWhiteSpace(c))) return false;
        return Uri.TryCreate(url, UriKind.Absolute, out var parsed)
            && parsed.Scheme == Uri.UriSchemeHttps
            && parsed.UserInfo.Length == 0
            && url.IndexOf('@', StringComparison.Ordinal) < 0
            && string.Equals(parsed.IdnHost, core.IdnHost, StringComparison.OrdinalIgnoreCase);
    }
}
