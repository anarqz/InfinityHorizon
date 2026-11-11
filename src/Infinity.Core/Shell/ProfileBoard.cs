// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Saves;

namespace Infinity.Core.Shell;

/// <summary>Where a profile's career of the chosen game stands on the Profiles page (OD-214, CAREERS.md §4, §6.7).</summary>
public enum CareerState
{
    /// <summary>The career exists and has been played or saved: choosing the card selects it.</summary>
    Started,

    /// <summary>
    /// The career exists but nothing started it (a game with cloud saves, no revision, no sync, never played; §6.7 "an empty career is
    /// not started"): the Start card comes back (Fresh, from an existing profile, or delete the career).
    /// </summary>
    Empty,

    /// <summary>No career of the game: the card offers to start one.</summary>
    None,

    /// <summary>No career, and the game allows none for the profile's mode (FM5 Hardcore until M11, §6.2).</summary>
    NotOffered,
}

/// <summary>What a career's progress line says (CAREERS.md §6.4; handoff W12 "Level 0 is real").</summary>
public enum CareerProgressKind
{
    /// <summary><c>progress</c> absent: the career has not synced yet.</summary>
    NoSyncYet,

    /// <summary><c>progress.level</c> 0: a new career.</summary>
    NewCareer,

    /// <summary>Level, credits, cars and play time as the server has them.</summary>
    Progress,

    /// <summary>OD-239: an Azimuth career plays the player's own Azimuth save; the server keeps no progress of it (<c>progress</c> absent).</summary>
    AzimuthSave,
}

/// <summary>The server facts the board reads beside <c>/me</c>.</summary>
/// <param name="HardcoreFromProfile"><c>ServerInfo.saves.hardcoreFromProfile</c> (SAVES_API.md §4.10, OD-214).</param>
/// <param name="HardcoreKeepsCloud">A Hardcore profile of this account keeps a cloud line on this server (OD-52).</param>
/// <param name="SeedPendingXuids">Profiles whose career of the game holds an accepted seed waiting for its first Play on this PC (a pending <c>seed_from_profile</c>): they count as started, never as empty (review M1).</param>
public sealed record BoardFlags(bool HardcoreFromProfile, bool HardcoreKeepsCloud, IReadOnlySet<string>? SeedPendingXuids = null);

/// <summary>
/// One profile on the Profiles page of the chosen game: the <see cref="Player"/>, its career of the game (<see cref="Career"/> and, joined by
/// <c>careerId</c>, that career's <see cref="Profile"/> row with its switches, §6.4), where it stands, what starting it offers, and whether
/// the account holds another live profile of the same mode (§6.10: until the owner's cleanup, every one is listed).
/// </summary>
public sealed record BoardCard(Player Player, Career? Career, Profile? CareerRow, CareerState State, CareerOffers Offers, bool Duplicate)
{
    /// <summary>The career's id: the gate's profile key once chosen (CR-S98 §6.3); null without a career.</summary>
    public string? CareerId => Career?.CareerId;

    /// <summary>A card the player can pick to play now.</summary>
    public bool Playable => State == CareerState.Started && CareerRow is not null;

    /// <summary>A card that opens the Start panel.</summary>
    public bool Startable => State is CareerState.None or CareerState.Empty && Offers.Any;

    public CareerProgressKind ProgressKind => Player.Mode == "hardcore" ? ProfileBoard.ProgressKind(Career) : CareerProgressKind.AzimuthSave;

    /// <summary>An accepted seed waits for this career's first Play on this PC (review M1): started, not empty.</summary>
    public bool SeedPending { get; init; }
}

/// <summary>
/// OD-214 (CAREERS.md §6.18): sign in → pick a game → pick a profile. The Profiles page lists the account's profiles (<c>players</c>, both
/// modes) with the chosen game's career on each. A pure function of <c>/me</c>, the game and the server's flags, so the App only renders it.
/// </summary>
public static class ProfileBoard
{
    /// <summary>
    /// The cards of <paramref name="game"/>: one per live <see cref="Player"/>, oldest first as <c>/me</c> lists them. Against a server
    /// before CR-S98 (<c>players</c> absent, §6.11) one card per live <c>profiles</c> row of the game, each started.
    /// </summary>
    public static IReadOnlyList<BoardCard> Build(Me me, GameDefinition game, BoardFlags flags)
    {
        ArgumentNullException.ThrowIfNull(me);
        ArgumentNullException.ThrowIfNull(game);
        ArgumentNullException.ThrowIfNull(flags);
        var players = me.Players is { } listed
            ? listed.Where(p => p.Status == "active").ToList()
            : me.Profiles.Where(p => p.Game == game.Id && p.Status == "active").Select(FromProfile).ToList();
        var modes = players.GroupBy(p => p.Mode, StringComparer.Ordinal).ToDictionary(g => g.Key, g => g.Count(), StringComparer.Ordinal);
        var cards = new List<BoardCard>(players.Count);
        foreach (var player in players)
        {
            var career = player.Careers.FirstOrDefault(c => c.Game == game.Id);
            var row = career is null ? null : me.Profiles.FirstOrDefault(p => p.ProfileId == career.CareerId);
            var seedPending = flags.SeedPendingXuids?.Contains(player.Xuid) == true && career is not null && row is not null;
            var state = StateOf(player, career, game, seedPending);
            var offers = state is CareerState.None or CareerState.Empty
                ? CareerStart.Offers(player.Mode, game, flags, ProfileSeed.Sources(me.Profiles, game.Id, player.Xuid))
                : CareerOffers.Nothing;
            cards.Add(new BoardCard(player, career, row, state, offers, modes[player.Mode] > 1) { SeedPending = seedPending });
        }
        return cards;
    }

    /// <summary>Where <paramref name="player"/>'s career of <paramref name="game"/> stands.</summary>
    /// <remarks>
    /// OD-239: an Azimuth career (wire mode <c>noob</c>) that exists is always started: it plays the player's own Azimuth save, so there is
    /// nothing to start it from and no cloud revision or <c>progress</c> to wait for. A Hardcore career is not started by a local save
    /// (OD-60: it starts from the beginning or through the §4.10 check).
    /// </remarks>
    public static CareerState StateOf(Player player, Career? career, GameDefinition game, bool seedPending = false)
    {
        ArgumentNullException.ThrowIfNull(player);
        ArgumentNullException.ThrowIfNull(game);
        if (career is null)
        {
            return player.Mode == "hardcore" && !game.HardcoreCareers ? CareerState.NotOffered : CareerState.None;
        }
        if (player.Mode != "hardcore") return CareerState.Started;
        // §6.7: on a game with cloud saves, a Hardcore career nothing started (no revision, no sync, never played) is not started in the UI
        return game.FullFeatures && !seedPending && career is { Cloud: null, Progress: null, LastPlayedAt: null } ? CareerState.Empty : CareerState.Started;
    }

    /// <summary>What the progress line of <paramref name="career"/> says.</summary>
    public static CareerProgressKind ProgressKind(Career? career) => career?.Progress switch
    {
        null => CareerProgressKind.NoSyncYet,
        { Level: 0 } => CareerProgressKind.NewCareer,
        _ => CareerProgressKind.Progress,
    };

    /// <summary>The card a pre-CR-S98 <c>profiles</c> row stands for: its own profile with one started career.</summary>
    private static Player FromProfile(Profile p) =>
        new(p.ProfileId, p.Gamertag, p.Mode, p.Xuid, p.Status, p.RenameNotice,
            [new Career(p.Game, p.ProfileId, p.CreatedAt, p.LastPlayedAt ?? p.CreatedAt, null, null)]);
}
