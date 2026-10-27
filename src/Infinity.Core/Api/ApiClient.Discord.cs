// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Api;

/// <summary>
/// Core's player Discord routes (CORE_API.md §5a.2, register CR-S63) with the launcher access token, on core's origin. None takes an
/// <c>Idempotency-Key</c>: a new link code only replaces the old one, the unlink and the switch are idempotent.
/// </summary>
public sealed partial class ApiClient
{
    /// <summary><c>GET /core/v1/me/discord</c> → 200 <see cref="DiscordSettings"/>.</summary>
    public Task<DiscordSettings> DiscordSettingsAsync(string access, CancellationToken cancellation = default) =>
        SendAsync<DiscordSettings>(HttpMethod.Get, Core, "/core/v1/me/discord", null, access, idempotent: false, cancellation);

    /// <summary><c>POST /core/v1/me/discord/link</c> (no body) → 201 <see cref="DiscordLinkCode"/>.</summary>
    public Task<DiscordLinkCode> CreateDiscordLinkAsync(string access, CancellationToken cancellation = default) =>
        SendAsync<DiscordLinkCode>(HttpMethod.Post, Core, "/core/v1/me/discord/link", null, access, idempotencyKey: null, cancellation);

    /// <summary><c>DELETE /core/v1/me/discord/link</c> → 204, also when not linked.</summary>
    public async Task UnlinkDiscordAsync(string access, CancellationToken cancellation = default) =>
        await SendAsync<JsonElement?>(HttpMethod.Delete, Core, "/core/v1/me/discord/link", null, access, idempotencyKey: null, cancellation).ConfigureAwait(false);

    /// <summary><c>PUT /core/v1/me/profiles/{xuid}/announce</c> → 200 <see cref="ProfileAnnounce"/>.</summary>
    public Task<ProfileAnnounce> SetAnnounceAsync(string access, string xuid, bool announce, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(xuid);
        return SendAsync<ProfileAnnounce>(HttpMethod.Put, Core, $"/core/v1/me/profiles/{Uri.EscapeDataString(xuid)}/announce",
            new ProfileAnnounceUpdate(announce), access, idempotencyKey: null, cancellation);
    }
}
