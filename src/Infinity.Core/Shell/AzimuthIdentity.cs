// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;

namespace Infinity.Core.Shell;

/// <summary>One profile that could be the account's Azimuth identity: its id, its last session end (null: never played) and its creation.</summary>
public sealed record AzimuthCandidate(string ProfileId, DateTimeOffset? LastPlayedAt, DateTimeOffset CreatedAt);

/// <summary>
/// OD-240: one Azimuth identity per account. The player picks its gamertag once, at first sign-in (the account's single <c>noob</c> profile on
/// the server); the launcher keeps it behind the scenes: nothing to create, choose or delete, and PLAY AZIMUTH plays this PC's
/// <c>User_9000000000000</c> under it. An account from before CR-S98 may hold several <c>noob</c> profiles: the identity is the one played
/// last (the newest <c>profiles[].lastPlayedAt</c> of <c>GET /launcher/v1/me</c>, the last session's end); on a tie, or when none was
/// played, the oldest by <c>createdAt</c>. Offline (no account) it is this PC's own local Azimuth profile (<see cref="Local"/>).
/// </summary>
public static class AzimuthIdentity
{
    /// <summary>OD-240: what a refused Azimuth (noob) create says: the account already has its identity; never retried under another name.</summary>
    public const string ExistsKey = "profiles.azimuth_identity_exists";

    /// <summary>
    /// The string key for a refused profile create, when it is the identity rule: <c>ACCOUNTS.PROFILE_LIMIT</c> on a non-Hardcore
    /// create (CAREERS.md §2: one live noob profile per account) is <see cref="ExistsKey"/>; null for anything else.
    /// </summary>
    public static string? RefusalKey(string? code, bool hardcore) => !hardcore && code == ErrorCodes.AccountsProfileLimit ? ExistsKey : null;

    /// <summary>The rule: the newest <see cref="AzimuthCandidate.LastPlayedAt"/>; a tie or none set, the oldest <see cref="AzimuthCandidate.CreatedAt"/>; then the id.</summary>
    public static string? Pick(IEnumerable<AzimuthCandidate> candidates)
    {
        ArgumentNullException.ThrowIfNull(candidates);
        return candidates
            .OrderByDescending(c => c.LastPlayedAt ?? DateTimeOffset.MinValue)
            .ThenBy(c => c.CreatedAt)
            .ThenBy(c => c.ProfileId, StringComparer.Ordinal)
            .FirstOrDefault()?.ProfileId;
    }

    /// <summary>
    /// The account's Azimuth identity on a CR-S98 server: one of its live <c>noob</c> players, each read with the newest
    /// <c>lastPlayedAt</c> and the oldest <c>createdAt</c> of its careers' <c>profiles[]</c> rows (its careers' own times when a row is
    /// missing). Null when the account has none (the player then picks its gamertag once).
    /// </summary>
    public static Player? Of(Me me)
    {
        ArgumentNullException.ThrowIfNull(me);
        if (me.Players is not { } players) return null;
        var live = players.Where(p => p is { Mode: "noob", Status: "active" }).ToList();
        var candidates = live.Select(player =>
        {
            var ids = player.Careers.Select(c => c.CareerId).ToHashSet(StringComparer.Ordinal);
            var rows = me.Profiles.Where(r => ids.Contains(r.ProfileId)).ToList();
            var played = rows.Select(r => Time(r.LastPlayedAt)).Concat(player.Careers.Where(c => rows.All(r => r.ProfileId != c.CareerId)).Select(c => Time(c.LastPlayedAt)))
                .Where(t => t is not null).Max();
            var created = rows.Select(r => Time(r.CreatedAt)).Concat(player.Careers.Select(c => Time(c.CreatedAt))).Where(t => t is not null).Min();
            return new AzimuthCandidate(player.ProfileId, played, created ?? DateTimeOffset.MaxValue);
        });
        return Pick(candidates) is { } id ? live.First(p => p.ProfileId == id) : null;
    }

    /// <summary>The same rule over plain <c>profiles</c> rows (a server before CR-S98: each row of the game is its profile).</summary>
    public static Profile? OfRows(IEnumerable<Profile> rows)
    {
        ArgumentNullException.ThrowIfNull(rows);
        var live = rows.Where(r => r is { Mode: "noob", Status: "active" }).ToList();
        return Pick(live.Select(r => new AzimuthCandidate(r.ProfileId, Time(r.LastPlayedAt), Time(r.CreatedAt) ?? DateTimeOffset.MaxValue))) is { } id
            ? live.First(r => r.ProfileId == id) : null;
    }

    /// <summary>
    /// Whether the account already has its Azimuth identity for <paramref name="game"/>: on a CR-S98 server any live <c>noob</c> player (the
    /// identity is the account's, whatever game its careers are of); before CR-S98 a live <c>noob</c> row of the game. With one, a local
    /// Azimuth profile is never uploaded (it becomes the identity on this PC), so the server never holds a second <c>noob</c> profile.
    /// </summary>
    public static bool Exists(Me me, string game)
    {
        ArgumentNullException.ThrowIfNull(me);
        return me.Players is not null ? Of(me) is not null : OfRows(me.Profiles.Where(p => p.Game == game)) is not null;
    }

    /// <summary>
    /// The identity's <c>profiles[]</c> row of <paramref name="game"/> (its career there), null without an identity or without that career yet
    /// (PLAY AZIMUTH makes it). This PC's copy of that row is the one local entry of the identity.
    /// </summary>
    public static Profile? RowOf(Me me, string game)
    {
        ArgumentNullException.ThrowIfNull(me);
        if (me.Players is null) return OfRows(me.Profiles.Where(p => p.Game == game));
        if (Of(me)?.Careers.FirstOrDefault(c => c.Game == game) is not { } career) return null;
        return me.Profiles.FirstOrDefault(p => p.ProfileId == career.CareerId && p.Game == game);
    }

    /// <summary>
    /// Offline (no account): this PC's one local Azimuth identity of <paramref name="game"/>, among its local <c>noob</c> profiles. The cached
    /// copy of the identity of the last sign-in (<paramref name="lastIdentityXuid"/>, its account XUID) comes first, so offline plays under the
    /// same gamertag as online; then any copy of an account profile the account still lists; then the oldest created; then the id. Null when
    /// this PC has none (PLAY AZIMUTH then makes one behind the scenes, named after Azimuth's own player when that name is a valid gamertag,
    /// else it asks for the name once).
    /// </summary>
    public static LocalProfile? Local(IEnumerable<LocalProfile> locals, string game, string? lastIdentityXuid = null)
    {
        ArgumentNullException.ThrowIfNull(locals);
        ArgumentNullException.ThrowIfNull(game);
        return locals.Where(l => l.Game == game && l.Mode == "noob")
            .OrderBy(l => lastIdentityXuid is not null && l.Cloud?.Xuid == lastIdentityXuid && !l.NotOnAccount ? 0 : 1)
            .ThenBy(l => l.Cloud is not null && !l.NotOnAccount ? 0 : 1)
            .ThenBy(l => Time(l.CreatedAt) ?? DateTimeOffset.MaxValue)
            .ThenBy(l => l.Id, StringComparer.Ordinal)
            .FirstOrDefault();
    }

    private static DateTimeOffset? Time(string? at) =>
        DateTimeOffset.TryParse(at, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var t) ? t : null;
}
