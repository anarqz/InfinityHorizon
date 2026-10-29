// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;

namespace Infinity.Core.Api;

// CR-S72 (OD-136): ACCOUNTS.md §5 "Presence". Mirrors of fi_protocol::accounts::{ProfilePresence, PresenceClub} /
// packages/contracts/src/accounts.ts (ContractParityTests).

/// <summary>A club in <see cref="ProfilePresence"/>: <see cref="Tag"/> is 2 to 4 of <c>A-Z0-9</c>, <see cref="Name"/> 1 to 24 characters.</summary>
public sealed record PresenceClub(string Tag, string Name);

/// <summary>
/// <c>GET /launcher/v1/profiles/{xuid}/presence</c> → 200: the profile's Car Club of its current mode, and its level and credits from the
/// last accepted <c>UpdateAll</c> of that mode (<see cref="SyncedAt"/> its time, RFC 3339). Each field is absent when unknown
/// (<c>{}</c> before any club or accepted sync).
/// </summary>
public sealed record ProfilePresence(PresenceClub? Club = null, uint? Level = null, ulong? Credits = null, string? SyncedAt = null);

public sealed partial class ApiClient
{
    /// <summary>
    /// CR-S72 with the access token, the caller's own profile only. Read at game start and every 5 minutes, never faster: past 60 reads
    /// per account per hour it is 429 <c>REQUEST.RATE_LIMITED</c> with <c>Retry-After</c> (thrown; the caller honours it). Null on 404
    /// (<c>ACCOUNTS.NOT_FOUND</c>: not this account's profile, or a server without the route) and 405: no club, and level and
    /// credits only from the hook.
    /// </summary>
    public async Task<ProfilePresence?> ProfilePresenceAsync(string access, string xuid, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(xuid);
        try
        {
            return await SendAsync<ProfilePresence>(HttpMethod.Get, Core, $"/launcher/v1/profiles/{Uri.EscapeDataString(xuid)}/presence", null, access,
                idempotent: false, cancellation).ConfigureAwait(false);
        }
        catch (ApiException unsupported) when (unsupported.Status is HttpStatusCode.NotFound or HttpStatusCode.MethodNotAllowed)
        {
            return null;
        }
    }
}
