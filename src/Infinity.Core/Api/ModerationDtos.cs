// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json.Serialization;

namespace Infinity.Core.Api;

// The role routes the launcher's moderation menu uses (CORE_API.md §5; register CR-S62). Mirrors of fi-protocol's
// core.rs / packages/contracts/src/core.ts (ContractParityTests), except AccountLookup, which has no TypeScript mirror.

/// <summary>
/// <c>POST /core/v1/admin/kicks</c> (MOD, ADM): exactly one of <see cref="Xuid"/> and <see cref="AccountId"/>; the other is left out of
/// the body, never sent as <c>null</c>. <see cref="CooldownMinutes"/> 0 to 60.
/// </summary>
public sealed record NewKick(
    [property: JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] string? Xuid,
    [property: JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] string? AccountId,
    string Reason,
    [property: JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] int? CooldownMinutes);

/// <summary>The kick's result: the live play sessions it ended and the cooldown in force (<c>null</c>: none).</summary>
public sealed record KickApplied(string AccountId, ulong SessionsEnded, string? Until);

/// <summary><c>POST /core/v1/admin/strikes</c> (MOD, ADM): a manual strike on a profile.</summary>
public sealed record NewStrike(string Xuid, string Reason)
{
    [JsonPropertyOrder(1)]
    public string Source { get; init; } = "manual";
}

public sealed record StrikeReview(string At, string Outcome);

/// <summary><c>POST /core/v1/admin/strikes</c> → 201.</summary>
public sealed record Strike(string Id, string Game, string Xuid, string Source, string Reason, string CreatedAt, StrikeReview? Reviewed);

/// <summary><c>POST /core/v1/admin/bans</c> (ADM): <see cref="Until"/> <c>null</c> is a permanent ban and is sent as <c>null</c>.</summary>
public sealed record NewBan(string AccountId, string? Until, string Reason);

public sealed record BanApplied(string AccountId, string? Until);

/// <summary>One match of <c>GET /core/v1/admin/accounts?gamertag=|xuid=</c> (fi-protocol <c>AccountLookup</c>).</summary>
public sealed record AccountLookup(string AccountId, string Gamertag, string Game, string Xuid);

public sealed record AccountLookupList(IReadOnlyList<AccountLookup> Items);
