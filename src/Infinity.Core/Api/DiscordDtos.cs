// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Api;

// The player's Discord routes (CORE_API.md §5a.2; register CR-S63). Mirrors of fi-protocol's core.rs / packages/contracts/src/core.ts
// (ContractParityTests). Discord ids are decimal strings, never numbers.

/// <summary>The account's verified Discord link; <see cref="AvatarUrl"/> is the account's avatar on Discord's CDN.</summary>
public sealed record DiscordLink(string DiscordUserId, string Username, string? GlobalName, string AvatarUrl, string LinkedAt, string RefreshedAt);

/// <summary>A live profile's announce switch (on by default): whether the bot may name it in #activity.</summary>
public sealed record ProfileAnnounce(string Xuid, string Game, string Gamertag, bool Announce);

/// <summary><c>PUT /core/v1/me/profiles/{xuid}/announce</c>.</summary>
public sealed record ProfileAnnounceUpdate(bool Announce);

/// <summary><c>GET /core/v1/me/discord</c> → 200. <see cref="Available"/> false: the server offers no linking.</summary>
public sealed record DiscordSettings(bool Available, DiscordLink? Link, IReadOnlyList<ProfileAnnounce> Profiles);

/// <summary><c>POST /core/v1/me/discord/link</c> → 201: the bot's page to open (one-time code, 10 minutes).</summary>
public sealed record DiscordLinkCode(string Url, string ExpiresAt);
