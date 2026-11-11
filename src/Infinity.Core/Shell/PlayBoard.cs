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
    /// <remarks>
    /// OD-240: AZIMUTH is one identity per account and plays this PC's Azimuth save, so its button is PLAY AZIMUTH whenever the identity
    /// exists (a game without its career yet gets one behind the scenes at the click); without one the player picks its gamertag once.
    /// </remarks>
    public static PlayButtonKind Kind(GameDefinition game, string mode, bool profileExists, CareerState? career, bool needsResolve)
    {
        if (mode != "hardcore") return profileExists ? PlayButtonKind.Play : PlayButtonKind.CreateProfile;
        if (!ModeAllowed(game, mode) || career == CareerState.NotOffered) return PlayButtonKind.NotOffered;
        if (!profileExists) return PlayButtonKind.CreateProfile;
        return career switch
        {
            CareerState.None or CareerState.Empty => PlayButtonKind.StartCareer,
            _ => needsResolve ? PlayButtonKind.Resolve : PlayButtonKind.Play,
        };
    }

    /// <summary>
    /// The card a mode's button stands for. HARDCORE: until the owner's cleanup an account may hold two profiles of a mode (§6.10); the one
    /// with a playable career of the game wins, then one that can start it, then the oldest. AZIMUTH (OD-240): the account's one identity,
    /// <paramref name="azimuthProfileId"/> (<see cref="AzimuthIdentity.Of"/>), never another <c>noob</c> profile.
    /// </summary>
    public static BoardCard? CardOf(IEnumerable<BoardCard> cards, string mode, string? azimuthProfileId = null)
    {
        ArgumentNullException.ThrowIfNull(cards);
        var ofMode = cards.Where(c => c.Player.Mode == mode).ToList();
        if (mode != "hardcore") return ofMode.FirstOrDefault(c => c.Player.ProfileId == azimuthProfileId);
        return ofMode.FirstOrDefault(c => c.Playable) ?? ofMode.FirstOrDefault(c => c.Startable) ?? ofMode.FirstOrDefault();
    }

    /// <summary>
    /// Both buttons for <paramref name="game"/> from the cards of <c>/me</c> (<paramref name="needsResolve"/>: the card's sync chip asks the
    /// player; <paramref name="azimuthProfileId"/>: the account's Azimuth identity).
    /// </summary>
    public static IReadOnlyList<PlaySlot> Build(IReadOnlyList<BoardCard> cards, GameDefinition game, Func<BoardCard, bool>? needsResolve = null, string? azimuthProfileId = null)
    {
        ArgumentNullException.ThrowIfNull(cards);
        ArgumentNullException.ThrowIfNull(game);
        return Modes.Select(mode =>
        {
            var card = CardOf(cards, mode, azimuthProfileId);
            var resolve = mode == "hardcore" && card is { State: CareerState.Started } && needsResolve?.Invoke(card) == true;
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
/// Owner 2026-11-08 (PROFILES_ROWS), T-1111-AZIMUTH (the prototype's two peer cards): the Profiles page's ⋯ menus. A card's ⋯ holds only the
/// profile's own actions (<see cref="ProfileLevel"/>; the AZIMUTH card only Help Sensei, its one identity has nothing to rename or delete);
/// each game row under the HARDCORE card has its own ⋯ with that game's CAREER actions (<see cref="CareerLevel"/>), and each row under the
/// AZIMUTH card the tools of this PC's Azimuth save of that game (<see cref="AzimuthLevel"/>: what "Manage saves" held, plus the old Noob saves
/// of OD-240 item 5). Every one opens the existing screen with the row's game set. No tool plays or starts a career.
/// </summary>
public static class ProfileTools
{
    public const string History = "history";
    public const string Resolve = "resolve";
    public const string CopyTo = "copy-azimuth";
    public const string Download = "download";
    public const string HelpSensei = "help";
    public const string Rename = "rename";
    public const string DeleteCareer = "delete-career";
    public const string DeleteProfile = "delete-profile";
    /// <summary>
    /// An AZIMUTH row (owner 2026-11-11): "Use another save on this PC…", the picker of every save container of the game on this PC (old Noob
    /// folders, HARDCORE folders, unknown ones, the launcher's backups); the pick becomes the Azimuth save after a verified backup.
    /// </summary>
    public const string UseOtherSave = "use-other-save";
    /// <summary>An AZIMUTH row: Azimuth's original save of the game back (what is there backed up first).</summary>
    public const string RestoreOriginal = "restore-original";
    /// <summary>An AZIMUTH row, one per old Noob cloud save of the game (<c>old-copy:&lt;key&gt;</c>): download it, back up, copy it into the Azimuth save.</summary>
    public const string OldCopy = "old-copy";
    /// <summary>An AZIMUTH row, an old Noob cloud save (<c>old-export:&lt;key&gt;</c>): export it to a file.</summary>
    public const string OldExport = "old-export";


    /// <summary>The profile card's tools, in the menu's order.</summary>
    public static IReadOnlyList<string> ProfileLevel { get; } = [DeleteProfile, Rename, HelpSensei];

    /// <summary>A HARDCORE game row's career tools, in the menu's order (owner 2026-11-08).</summary>
    public static IReadOnlyList<string> CareerLevel { get; } = [CopyTo, History, Resolve, Download, DeleteCareer];

    /// <summary>An AZIMUTH game row's tools, in the menu's order (the old-save ones once per old save of the game).</summary>
    public static IReadOnlyList<string> AzimuthLevel { get; } = [UseOtherSave, History, RestoreOriginal, OldCopy, OldExport];

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

    /// <summary>One HARDCORE game row's menu: the career tools that apply to it, in order (whether each is enabled is the page's).</summary>
    /// <param name="started">The profile has a started career of the row's game (its saves can be copied, resolved, downloaded).</param>
    /// <param name="canDownload">Its sync chip allows a download onto this PC (a game with cloud saves).</param>
    /// <param name="canDeleteCareer">A career of the game exists to delete.</param>
    public static IReadOnlyList<string> CareerMenu(bool started, bool canDownload, bool canDeleteCareer)
    {
        var list = new List<string>();
        if (started) list.Add(CopyTo);
        list.Add(History);
        if (started) list.Add(Resolve);
        if (started && canDownload) list.Add(Download);
        if (canDeleteCareer) list.Add(DeleteCareer);
        return list;
    }

    /// <summary>
    /// One AZIMUTH game row's menu, in order: Use another save on this PC…, History, Restore Azimuth's original, then for each old Noob cloud
    /// save of the game its copy and its export to a file. Whether each is enabled is the page's (the game running, no original recorded).
    /// </summary>
    /// <param name="oldCloudSaves">The keys of the game's old Noob cloud saves.</param>
    public static IReadOnlyList<string> AzimuthMenu(IReadOnlyList<string> oldCloudSaves)
    {
        ArgumentNullException.ThrowIfNull(oldCloudSaves);
        var list = new List<string> { UseOtherSave, History, RestoreOriginal };
        foreach (var key in oldCloudSaves)
        {
            list.Add(OldCopy + ":" + key);
            list.Add(OldExport + ":" + key);
        }
        return list;
    }
    /// <summary>The tool of an AZIMUTH menu id (<c>old-copy:&lt;key&gt;</c> is <see cref="OldCopy"/>) and its old save's key (null for the others).</summary>
    public static (string Tool, string? Key) Split(string id)
    {
        ArgumentNullException.ThrowIfNull(id);
        var at = id.IndexOf(':', StringComparison.Ordinal);
        return at < 0 ? (id, null) : (id[..at], id[(at + 1)..]);
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
