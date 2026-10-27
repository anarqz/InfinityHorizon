// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Api;

/// <summary>
/// Core's role routes for the launcher's moderation menu (CORE_API.md §5, register CR-S62), with the launcher access token. The
/// server checks the role on every call (<c>AUTH.ROLE_REQUIRED</c>); every write carries the caller's <c>Idempotency-Key</c>, kept
/// for the retries of one action after a lost answer.
/// </summary>
public sealed partial class ApiClient
{
    /// <summary><c>POST /core/v1/admin/kicks</c> → 200 <see cref="KickApplied"/>.</summary>
    public Task<KickApplied> KickAsync(string access, NewKick kick, string idempotencyKey, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(kick);
        ArgumentException.ThrowIfNullOrWhiteSpace(idempotencyKey);
        return SendAsync<KickApplied>(HttpMethod.Post, Core, "/core/v1/admin/kicks", kick, access, idempotencyKey, cancellation);
    }

    /// <summary><c>POST /core/v1/admin/strikes</c> → 201 <see cref="Strike"/>.</summary>
    public Task<Strike> StrikeAsync(string access, NewStrike strike, string idempotencyKey, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(strike);
        ArgumentException.ThrowIfNullOrWhiteSpace(idempotencyKey);
        return SendAsync<Strike>(HttpMethod.Post, Core, "/core/v1/admin/strikes", strike, access, idempotencyKey, cancellation);
    }

    /// <summary><c>POST /core/v1/admin/bans</c> (ADM) → 201 <see cref="BanApplied"/>.</summary>
    public Task<BanApplied> BanAsync(string access, NewBan ban, string idempotencyKey, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(ban);
        ArgumentException.ThrowIfNullOrWhiteSpace(idempotencyKey);
        return SendAsync<BanApplied>(HttpMethod.Post, Core, "/core/v1/admin/bans", ban, access, idempotencyKey, cancellation);
    }

    /// <summary>
    /// <c>GET /core/v1/admin/accounts?xuid=</c> (a decimal XUID) or <c>?gamertag=</c> (anything else): the accounts a ban names
    /// (MOD, ADM; never by e-mail here).
    /// </summary>
    public Task<AccountLookupList> LookupAccountsAsync(string access, string xuidOrGamertag, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(xuidOrGamertag);
        var value = xuidOrGamertag.Trim();
        var query = Shell.ModerationRequests.IsXuid(value) ? "xuid=" : "gamertag=";
        return SendAsync<AccountLookupList>(HttpMethod.Get, Core, "/core/v1/admin/accounts?" + query + Uri.EscapeDataString(value), null, access,
            idempotent: false, cancellation);
    }
}
