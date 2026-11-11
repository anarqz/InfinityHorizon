// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Process;

namespace Infinity.Core.Shell;

/// <summary>What one of PLAY's two large buttons does for the selected game (owner 2026-11-08, the revamp's button states).</summary>
public enum PlayButtonKind
{
    /// <summary>The profile has a started career of the game: "PLAY HARDCORE" / "PLAY NOOB" launches it through the play path.</summary>
    Play,

    /// <summary>(1) The profile exists but has no career of the game (or an empty one): "START {MODE} CAREER" opens the inline choice.</summary>
    StartCareer,

    /// <summary>(2) No profile of the mode: "Create {MODE} profile" goes to Profiles.</summary>
    CreateProfile,

    /// <summary>(3) The game has no such mode (catalog data, e.g. <see cref="GameDefinition.HardcoreCareers"/> false): disabled, with the reason.</summary>
    NotOffered,

    /// <summary>(4) A sync chip that needs the player ("This PC has newer progress"...): the button opens Resolve.</summary>
    Resolve,
}

/// <summary>One of PLAY's buttons: the mode, what it does, and the profile card it stands for (null without a profile of the mode).</summary>
public sealed record PlaySlot(string Mode, PlayButtonKind Kind, BoardCard? Card)
{
    /// <summary>The career's progress line exists (level · credits · hours from the one <c>/me</c>); absent progress is "none yet".</summary>
    public bool HasProgress => Card?.Career?.Progress is not null && Card.State == CareerState.Started;
}

/// <summary>
/// Owner 2026-11-08 (the PLAY revamp): the selected game's two buttons, one per mode, as a pure function of the cards
/// <see cref="ProfileBoard"/> made from the single <c>/me</c> read at sign-in. Switching games rebuilds them from that same <c>/me</c>: no
/// request, no spinner. Game specifics are the catalog's data (<see cref="GameDefinition"/>), never a test on a game id.
/// </summary>
public static class PlayBoard
{
    /// <summary>The two buttons, left to right.</summary>
    public static IReadOnlyList<string> Modes { get; } = ["hardcore", "noob"];

    /// <summary>
    /// CR-S111 (OD-239): the string key of what a profile's save is, from its mode alone: an Azimuth profile plays the player's own
    /// Azimuth save; a Hardcore one, free or Supporter, keeps a cloud save, unless the server says this profile has no line
    /// (<c>Profile.cloudLine: false</c>).
    /// </summary>
    public static string SaveWordsKey(string mode, bool? cloudLine) =>
        mode != "hardcore" ? "modes.noob_own_save" : cloudLine ?? true ? "modes.hardcore_cloud" : "modes.hardcore_local";

    /// <summary>The game has careers of <paramref name="mode"/> (catalog data).</summary>
    public static bool ModeAllowed(GameDefinition game, string mode)
    {
        ArgumentNullException.ThrowIfNull(game);
        return mode != "hardcore" || game.HardcoreCareers;
    }

    /// <summary>
    /// The button's kind for (profile exists × career state × mode allowed × sync needs the player). <paramref name="career"/> null with a
    /// profile: a row without career data (offline, or a server before CR-S98), which is its one started career.
    /// </summary>
    public static PlayButtonKind Kind(GameDefinition game, string mode, bool profileExists, CareerState? career, bool needsResolve)
    {
        if (!ModeAllowed(game, mode) || career == CareerState.NotOffered) return PlayButtonKind.NotOffered;
        if (!profileExists) return PlayButtonKind.CreateProfile;
        return career switch
        {
            CareerState.None or CareerState.Empty => PlayButtonKind.StartCareer,
            _ => needsResolve ? PlayButtonKind.Resolve : PlayButtonKind.Play,
        };
    }

    /// <summary>
    /// The card a mode's button stands for: until the owner's cleanup an account may hold two profiles of a mode (§6.10); the one with a
    /// playable career of the game wins, then one that can start it, then the oldest.
    /// </summary>
    public static BoardCard? CardOf(IEnumerable<BoardCard> cards, string mode)
    {
        ArgumentNullException.ThrowIfNull(cards);
        var ofMode = cards.Where(c => c.Player.Mode == mode).ToList();
        return ofMode.FirstOrDefault(c => c.Playable) ?? ofMode.FirstOrDefault(c => c.Startable) ?? ofMode.FirstOrDefault();
    }

    /// <summary>Both buttons for <paramref name="game"/> from the cards of <c>/me</c> (<paramref name="needsResolve"/>: the card's sync chip asks the player).</summary>
    public static IReadOnlyList<PlaySlot> Build(IReadOnlyList<BoardCard> cards, GameDefinition game, Func<BoardCard, bool>? needsResolve = null)
    {
        ArgumentNullException.ThrowIfNull(cards);
        ArgumentNullException.ThrowIfNull(game);
        return Modes.Select(mode =>
        {
            var card = CardOf(cards, mode);
            var resolve = card is { State: CareerState.Started } && needsResolve?.Invoke(card) == true;
            return new PlaySlot(mode, Kind(game, mode, card is not null, card?.State, resolve), card);
        }).ToList();
    }

    /// <summary>
    /// Both buttons from plain rows (offline: this PC's local profiles of the game; a server before CR-S98: its profile rows), whose
    /// existence is the career: <paramref name="modesWithProfile"/> the modes that have one.
    /// </summary>
    public static IReadOnlyList<PlaySlot> BuildPlain(IReadOnlyCollection<string> modesWithProfile, GameDefinition game)
    {
        ArgumentNullException.ThrowIfNull(modesWithProfile);
        ArgumentNullException.ThrowIfNull(game);
        return Modes.Select(mode => new PlaySlot(mode, Kind(game, mode, modesWithProfile.Contains(mode), null, false), null)).ToList();
    }

    /// <summary>The game PLAY opens on: the last one selected while it is still offered, else the first offered (null: none).</summary>
    public static string? InitialGame(IEnumerable<string> offered, string? last)
    {
        ArgumentNullException.ThrowIfNull(offered);
        var list = offered.ToList();
        return last is not null && list.Contains(last, StringComparer.Ordinal) ? last : list.FirstOrDefault();
    }
}

/// <summary>
/// PLAY's selected game (owner 2026-11-08): chosen at sign-in or on the offline path from the last one (local settings,
/// <see cref="LaunchModeStore"/>), changed by a tile. Selecting reads and writes only this PC's settings: never a request.
/// </summary>
public sealed class PlaySelection(string stateDirectory)
{
    private readonly string _stateDirectory = stateDirectory ?? throw new ArgumentNullException(nameof(stateDirectory));

    /// <summary>The selected game id, null before <see cref="Open"/>.</summary>
    public string? Game { get; private set; }

    /// <summary>The game to open on among <paramref name="offered"/> (the last one, else the first).</summary>
    public string? Open(IEnumerable<string> offered)
    {
        Game = PlayBoard.InitialGame(offered, LaunchModeStore.ReadLastGame(_stateDirectory));
        return Game;
    }

    /// <summary>A tile: <paramref name="game"/> becomes the selected game and is remembered across launches. False when it is not offered.</summary>
    public bool Select(string game, IEnumerable<string> offered)
    {
        ArgumentException.ThrowIfNullOrEmpty(game);
        ArgumentNullException.ThrowIfNull(offered);
        if (!offered.Contains(game, StringComparer.Ordinal)) return false;
        Game = game;
        LaunchModeStore.WriteLastGame(_stateDirectory, game);
        return true;
    }
}

/// <summary>
/// Owner 2026-11-08 (PROFILES_ROWS): the Profiles page's two kinds of ⋯ menu. A profile card's ⋯ holds only the profile's own actions
/// (<see cref="ProfileLevel"/>); each game row under it has its own ⋯ with that game's CAREER actions (<see cref="CareerLevel"/>), which open
/// the existing screens with the row's game set. No tool plays or starts a career (OD-239: an AZIMUTH row's old "Start my HARDCORE profile
/// from this" seeded from the stale export-only cloud line, not the Azimuth save it plays, so it is gone; a Hardcore career still starts from
/// another profile on its own creation, CR-S52).
/// </summary>
public static class ProfileTools
{
    public const string ManageSaves = "manage";
    public const string History = "history";
    public const string Resolve = "resolve";
    public const string CopyTo = "copy-azimuth";
    public const string Download = "download";
    public const string HelpSensei = "help";
    public const string Rename = "rename";
    public const string DeleteCareer = "delete-career";
    public const string DeleteProfile = "delete-profile";

    /// <summary>The profile card's tools, in the menu's order.</summary>
    public static IReadOnlyList<string> ProfileLevel { get; } = [DeleteProfile, Rename, HelpSensei];

    /// <summary>A game row's career tools, in the menu's order (owner 2026-11-08).</summary>
    public static IReadOnlyList<string> CareerLevel { get; } = [CopyTo, History, Resolve, ManageSaves, Download, DeleteCareer];

    /// <summary>The profile card's menu (whether each is enabled is the page's).</summary>
    /// <param name="canRename">The server allows this profile a rename now (a name change notice): otherwise Rename is not listed.</param>
    /// <param name="canDeleteProfile">The whole profile can be deleted.</param>
    public static IReadOnlyList<string> ProfileMenu(bool canRename, bool canDeleteProfile)
    {
        var list = new List<string>();
        if (canDeleteProfile) list.Add(DeleteProfile);
        if (canRename) list.Add(Rename);
        list.Add(HelpSensei);
        return list;
    }

    /// <summary>One game row's menu: the career tools that apply to it, in order (whether each is enabled is the page's).</summary>
    /// <param name="started">The profile has a started career of the row's game (its saves can be copied, resolved, downloaded).</param>
    /// <param name="canDownload">Its sync chip allows a download onto this PC (a game with cloud saves).</param>
    /// <param name="canDeleteCareer">A career of the game exists to delete.</param>
    /// <param name="hardcore">
    /// The row is a Hardcore career. OD-239: an Azimuth career has no cloud save, so it lists no History, Resolve or Download, and it starts
    /// no other profile's career (its cloud line is export-only and stale); it can still copy to Azimuth.
    /// </param>
    public static IReadOnlyList<string> CareerMenu(bool started, bool canDownload, bool canDeleteCareer, bool hardcore = true)
    {
        var list = new List<string>();
        if (started) list.Add(CopyTo);
        if (hardcore) list.Add(History);
        if (started && hardcore) list.Add(Resolve);
        list.Add(ManageSaves);
        if (started && canDownload && hardcore) list.Add(Download);
        if (canDeleteCareer) list.Add(DeleteCareer);
        return list;
    }
}

/// <summary>One game row of a profile card (owner 2026-11-08): the catalog game and the profile's career of it (null card: the profile is not on the board of that game).</summary>
public sealed record ProfileGameFacts(GameDefinition Game, BoardCard? Card)
{
    /// <summary>The career has progress from <c>/me</c> ("LV 15 · CR 15,858 · 22h"); otherwise the row says "No career data yet".</summary>
    public bool HasCareerData => Card is { State: CareerState.Started, Career.Progress: not null };

    public bool Started => Card?.State == CareerState.Started;
}

/// <summary>
/// Owner 2026-11-08 (PROFILES_ROWS): under each profile card, one row per catalog game, in catalog order, from the single <c>/me</c> (no
/// request per row). <paramref name="flags"/> gives each game's board flags (this PC's files of that game).
/// </summary>
public static class ProfileGameRows
{
    public static IReadOnlyList<ProfileGameFacts> Build(Me me, string profileId, IEnumerable<GameDefinition> catalog, Func<GameDefinition, BoardFlags> flags)
    {
        ArgumentNullException.ThrowIfNull(me);
        ArgumentNullException.ThrowIfNull(catalog);
        ArgumentNullException.ThrowIfNull(flags);
        return [.. catalog.Select(game => new ProfileGameFacts(game,
            ProfileBoard.Build(me, game, flags(game)).FirstOrDefault(c => c.Player.ProfileId == profileId)))];
    }
}
