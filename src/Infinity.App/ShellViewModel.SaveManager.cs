// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Windows.Media;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// The prototype's palette: HARDCORE orange and AZIMUTH blue (the mode's one colour on PLAY, Profiles and every screen), with their soft
/// fills; <see cref="Good"/> is the prototype's green for a good state (synced, newer, done, online), never a mode.
/// </summary>
public static class SlotBrushes
{
    public static readonly SolidColorBrush Hardcore = Frozen(0xFF, 0xFF, 0x6A, 0x3D);
    public static readonly SolidColorBrush HardcoreSoft = Frozen(0x1F, 0xFF, 0x6A, 0x3D);
    public static readonly SolidColorBrush Good = Frozen(0xFF, 0x3D, 0xDC, 0x97);
    public static readonly SolidColorBrush GoodSoft = Frozen(0x1F, 0x3D, 0xDC, 0x97);
    public static readonly SolidColorBrush Azimuth = Frozen(0xFF, 0x5A, 0xA8, 0xFF);
    public static readonly SolidColorBrush AzimuthSoft = Frozen(0x1F, 0x5A, 0xA8, 0xFF);

    /// <summary>A mode's accent: HARDCORE orange; AZIMUTH (wire <c>noob</c> and the Azimuth save alike) the prototype's Azimuth blue, everywhere.</summary>
    public static SolidColorBrush Accent(SaveSlot slot) => slot == SaveSlot.Hardcore ? Hardcore : Azimuth;

    public static SolidColorBrush Soft(SaveSlot slot) => slot == SaveSlot.Hardcore ? HardcoreSoft : AzimuthSoft;

    private static SolidColorBrush Frozen(byte a, byte r, byte g, byte b)
    {
        var brush = new SolidColorBrush(Color.FromArgb(a, r, g, b));
        brush.Freeze();
        return brush;
    }
}

/// <summary>The open ⋯ menu: its owner (a <see cref="SaveCard"/> or a <see cref="ProfileGameRow"/>) and its items.</summary>
public sealed record FloatingMenuView(object Owner, IReadOnlyList<MenuAction> Items);

/// <summary>One action of a card's ⋯ menu: its words, whether it can run now, and why not.</summary>
public sealed record MenuAction(string Id, string Label, bool Enabled, string? Reason = null, bool Danger = false)
{
    public bool HasReason => !string.IsNullOrEmpty(Reason);
}

/// <summary>
/// One of the two peer cards of the save manager's home (the prototype's "the vanilla way" and "the sandbox way"): the HARDCORE profile, or
/// the AZIMUTH identity with this PC's Azimuth save of each game; without either, that mode's explainer.
/// </summary>
public sealed record SaveCard(SaveSlot Slot, string ModeLabel)
{
    /// <summary>The HARDCORE profile's row (null on the AZIMUTH card, whose identity is <see cref="Name"/>).</summary>
    public ProfileRow? Row { get; init; }
    /// <summary>The AZIMUTH identity's gamertag (OD-240: shown, never chosen); null on the HARDCORE card.</summary>
    public string? Name { get; init; }
    public Brush Accent => SlotBrushes.Accent(Slot);
    public Brush Soft => SlotBrushes.Soft(Slot);
    public bool IsAzimuth => Slot == SaveSlot.Azimuth;
    public bool IsProfile => Row is not null || Name is not null;
    public bool IsExplainer => !IsProfile;
    public string Gamertag => Row?.Gamertag ?? Name ?? string.Empty;
    public string Initial => Gamertag.Length > 0 ? Gamertag[..1].ToUpperInvariant() : "?";
    public string Hint { get; init; } = string.Empty;
    public bool Selected { get; init; }
    /// <summary>The profile's own ⋯ (Delete profile, Rename when allowed, Help Sensei): no career action (owner 2026-11-08).</summary>
    public bool MenuOpen { get; init; }
    public bool HasMenu => IsProfile && Menu.Count > 0;
    public IReadOnlyList<MenuAction> Menu { get; init; } = [];
    /// <summary>Owner 2026-11-08: one row per catalog game under the header, each with its career line, sync chip and career ⋯.</summary>
    public IReadOnlyList<ProfileGameRow> Games { get; init; } = [];
    public string Note { get; init; } = string.Empty;
    public bool HasNote => !string.IsNullOrEmpty(Note);
    // the explainer of a missing mode (§4: "HARDCORE missing (explainer + Create)")
    public string ExplainTitle { get; init; } = string.Empty;
    public string ExplainBody { get; init; } = string.Empty;
    public string CreateLabel { get; init; } = string.Empty;
    public bool CreateEnabled { get; init; }
    public string CreateNote { get; init; } = string.Empty;
    public bool HasCreateNote => !string.IsNullOrEmpty(CreateNote);
}

/// <summary>
/// Owner 2026-11-08 (PROFILES_ROWS), T-1111-AZIMUTH: one catalog game under a card, the same fields on both cards: the cover (the
/// Mount/Logo.png resolver, else the styled tile), the game's name, "LV 15 · CR 15,858 · modified Nov 4, 22:40" (an unknown field "—", no
/// save or career a clear empty state in the same place), that game's chip, and a ⋯ with its tools (a HARDCORE career's, or this PC's Azimuth
/// save's on the AZIMUTH card, <see cref="Azimuth"/>).
/// </summary>
public sealed record ProfileGameRow(string ProfileId, string GameId, string Name, string ShortCode, Brush Accent, Brush Soft, string Line)
{
    /// <summary>A row of the AZIMUTH card: its ⋯ holds this PC's Azimuth save tools of the game (<see cref="ProfileTools.AzimuthLevel"/>).</summary>
    public bool Azimuth { get; init; }
    public ImageSource? Cover { get; init; }
    public bool HasCover => Cover is not null;
    public bool HasCareerData { get; init; }
    /// <summary>The fields line's tooltip: the line, and why a value is "—" when it is (an AZIMUTH level that cannot be read).</summary>
    public string? LineTip { get; init; }

    public string LineTipText => LineTip ?? Line;
    public string ChipText { get; init; } = string.Empty;
    /// <summary>The chip's full sentence (its tooltip); <see cref="ChipText"/> is the short form that fits the row.</summary>
    public string ChipTip { get; init; } = string.Empty;
    public ChipTone ChipTone { get; init; }
    public bool HasChip => !string.IsNullOrEmpty(ChipText);
    public bool ChipGood => ChipTone == ChipTone.Good;
    public bool ChipWarn => ChipTone == ChipTone.Warn;
    public bool MenuOpen { get; init; }
    public IReadOnlyList<MenuAction> Menu { get; init; } = [];
    public bool HasMenu => Menu.Count > 0;
    internal string MenuKey => ProfileId + "|" + GameId;
}

/// <summary>A line of the History timeline: a local entry or a cloud revision.</summary>
public sealed record HistoryRow(string Origin, string Title, string Detail, bool Cloud)
{
    public HistoryItem? Item { get; init; }
    /// <summary>The newest save inside <see cref="Item"/> (<see cref="LocalHistory.NewestSave"/>, verified off the UI thread when the row was built).</summary>
    internal SaveProfile? Save { get; init; }
    public SaveRevision? Revision { get; init; }
    public bool Selected { get; init; }
    public string When { get; init; } = string.Empty;
}

/// <summary>A key/value line of a comparison card.</summary>
public sealed record CompareLine(string Key, string Value, bool Up = false, bool Down = false);

/// <summary>One side of Resolve or Copy.</summary>
public sealed record CompareSide(string Title, string Pill, bool Newer, IReadOnlyList<CompareLine> Lines)
{
    public string ActionLabel { get; init; } = string.Empty;
    public bool Recommended { get; init; }
}

/// <summary>A step of the HARDCORE check's list.</summary>
public sealed record CheckStepRow(string Text, CheckStepState State)
{
    public string Glyph => State switch { CheckStepState.Done => "✓", CheckStepState.Running => "…", CheckStepState.Failed => "✕", _ => "·" };
    public Brush GlyphBrush => State switch
    {
        CheckStepState.Done => SlotBrushes.Good,
        CheckStepState.Running => Waiting,
        CheckStepState.Failed => Bad,
        _ => Muted,
    };

    private static readonly SolidColorBrush Waiting = Freeze(Color.FromRgb(0xFF, 0xC2, 0x4B));
    private static readonly SolidColorBrush Bad = Freeze(Color.FromRgb(0xFF, 0x54, 0x68));
    private static readonly SolidColorBrush Muted = Freeze(Color.FromArgb(0x80, 0xF9, 0xEA, 0xEE));

    private static SolidColorBrush Freeze(Color c) { var b = new SolidColorBrush(c); b.Freeze(); return b; }
}

/// <summary>
/// SAVE_MANAGER_UX.md (OD-207) as the Profiles page under OD-214, owner 2026-11-08 (PROFILES_ROWS), T-1111-AZIMUTH (the prototype's two
/// peer cards): HARDCORE ("the vanilla way") and AZIMUTH ("the sandbox way") side by side, the same structure: mode, avatar, gamertag, a ⋯,
/// then one row per catalog game with the same fields (level · credits · modified, or an empty state) and its own ⋯. HARDCORE's rows carry
/// its career tools; AZIMUTH's (OD-240's one identity, nothing to create or delete) this PC's Azimuth save of each game, read locally, and
/// what "Manage saves" and Settings' "Old Noob saves" held: Copy from HARDCORE, History, Restore Azimuth's original, the old Noob saves.
/// A mode the account has none of is its explainer. A row's action sets its game (PLAY's selection follows) and opens the existing screen.
/// Every non-root screen has ‹ Back and a breadcrumb (Esc does the same). Copies write only Azimuth's save on this PC, each one archived first in the launcher's own history (§7); a copy into
/// a cloud career that exists, the server's per-revision reasons and Delete-from-history wait for the server (needs MACBOOK, see
/// docs/launcher/batch-1107/ITEM10_DESIGN_GAPS.md).
/// </summary>
public sealed partial class ShellViewModel
{
    private readonly SavesNav _nav = new();
    /// <summary>Owner 2026-11-08: the one open ⋯ menu (a card's or a game row's), drawn floating over the page.</summary>
    private readonly FloatingMenuState _menu = new();
    private ProfileRow? _subject;
    private SaveSlot _historySlot = SaveSlot.Azimuth;
    private HistoryRow? _historySelected;
    private CopyPlan? _copy;
    private string _doneText = string.Empty;
    private bool _gameRunning;
    private string _createMode = "hardcore";
    private ResolvePick _resolvePick;
    private readonly List<SaveRevision> _cloudHistory = [];

    /// <summary>A copy into Azimuth (this PC): from a profile's save on this PC, a history entry, a cloud revision, or Azimuth's original.</summary>
    private sealed record CopyPlan(SaveSlot From, string FromName, SaveProfile? Local, HistoryItem? Entry, SaveRevision? Cloud, ulong? CloudXuid, bool Original);

    /// <summary>Harness seam: the game shown as running without a process.</summary>
    internal bool SavesForceRunning { get; set; }

    public ObservableCollection<SaveCard> SaveCards { get; } = [];
    public ObservableCollection<HistoryRow> HistoryRows { get; } = [];

    private CultureInfo SavesCulture => CultureInfo.GetCultureInfo(S.Locale.StartsWith("pt", StringComparison.OrdinalIgnoreCase) ? "pt-BR" : "en-US");

    // ------------------------------------------------------------------ navigation (§3)

    /// <summary>The screen shown: a screen whose subject is gone falls back to the root.</summary>
    public SavesScreen SavesCurrent => _nav.Current switch
    {
        SavesScreen.Start when _startRow is null => SavesScreen.Home,
        SavesScreen.Delete when !ShowDeleteProfile => SavesScreen.Home,
        SavesScreen.Resolve when _subject is null => SavesScreen.Home,
        var s => s,
    };

    private bool On(SavesScreen screen) => ShowProfiles && !OfflineMode && SavesCurrent == screen;

    /// <summary>The save manager draws the Profiles page (signed in to a server that lists <c>players</c>); offline keeps the plain list.</summary>
    public bool ShowSaveManager => ShowProfiles && !OfflineMode && _session?.Me?.Players is not null;
    public bool ShowPlainProfiles => ShowProfiles && !ShowSaveManager;
    public bool ShowSavesHome => ShowSaveManager && SavesCurrent == SavesScreen.Home;
    public bool ShowSavesCreate => ShowSaveManager && SavesCurrent == SavesScreen.Create;
    public bool ShowSavesStart => ShowSaveManager && SavesCurrent == SavesScreen.Start;
    public bool ShowSavesResolve => ShowSaveManager && SavesCurrent == SavesScreen.Resolve;
    public bool ShowSavesPick => ShowSaveManager && SavesCurrent == SavesScreen.Pick;
    public bool ShowSavesHistory => ShowSaveManager && SavesCurrent == SavesScreen.History;
    public bool ShowSavesDelete => ShowSaveManager && SavesCurrent == SavesScreen.Delete;
    public bool ShowSavesHelp => ShowSaveManager && SavesCurrent == SavesScreen.Help;
    public bool ShowSavesDone => ShowSaveManager && SavesCurrent == SavesScreen.Done;
    public bool ShowHelpLink => ShowSaveManager && SavesCurrent != SavesScreen.Help;
    public bool SavesCanGoBack => SavesCurrent != SavesScreen.Home;

    private string ScreenTitle(SavesScreen screen) => S["sm.crumb." + screen.ToString().ToLowerInvariant()];

    /// <summary>The breadcrumb before the current screen ("FH2  ›  History  ›  "), empty at the root.</summary>
    public string SavesTrail
    {
        get
        {
            var trail = _nav.Trail;
            if (SavesCurrent == SavesScreen.Home) return string.Empty;
            // owner 2026-11-08: the save tools act on PLAY's selected game, which the breadcrumb names first
            return GameLabel + "  ›  " + string.Concat(trail.Take(trail.Count - 1).Select(s => ScreenTitle(s) + "  ›  "));
        }
    }

    public string SavesTrailLast => ScreenTitle(SavesCurrent);

    public void SavesBack()
    {
        PruneSaves();
        _menu.Close();
        switch (SavesCurrent)
        {
            case SavesScreen.Start: CancelCareerStart(); return; // pops itself
            case SavesScreen.Delete: CancelDeleteProfile(); return; // pops itself
        }
        if (!_nav.Back()) return;
        PruneSaves();
        if (SavesCurrent == SavesScreen.Home) FillSaveCards();
        ChangedSaves();
    }

    /// <summary>Screens whose subject went away (a finished start, a closed delete) leave the stack.</summary>
    private void PruneSaves()
    {
        while (_nav.CanGoBack && SavesCurrent != _nav.Current) _nav.Back();
    }

    /// <summary>A flow screen closed by its own action (Cancel, Not now): one step back if it is on top.</summary>
    private void SavesPop(SavesScreen screen)
    {
        if (_nav.Current == screen) _nav.Back();
        PruneSaves();
        if (SavesCurrent == SavesScreen.Home) FillSaveCards();
        ChangedSaves();
    }

    private void SavesGo(SavesScreen screen)
    {
        _menu.Close();
        PruneSaves();
        _nav.Push(screen);
        Message = null;
        ChangedSaves();
    }

    public void SavesHome()
    {
        _nav.Root();
        _startRow = null; _copy = null; _copyAsk = null; _subject = null; _menu.Close();
        FillSaveCards();
        ChangedSaves();
    }

    public void OpenSavesHelp() => SavesGo(SavesScreen.Help);


    // ------------------------------------------------------------------ home (§4)

    public string SavesSubtitle => S["sm.subtitle"];


    public bool ShowGameRunningNote => _gameRunning && ShowSaveManager;

    public string GameRunningText => S.Format("sm.running", ("game", GameLabel));

    /// <summary>
    /// For the screens (owner 2026-11-10: no process list on the UI thread): the last off-thread answer at once, and a fresh one from a
    /// worker; the save screens follow when it changed. A write asks <see cref="RefreshGameRunningNowAsync"/> and waits for it.
    /// </summary>
    private void RefreshGameRunning()
    {
        _gameRunning = SavesForceRunning || Playing || _azimuthRunning;
        _ = RefreshGameRunningNowAsync(notify: true);
    }

    /// <summary>Whether azimuth.exe runs, read now on a worker.</summary>
    private async Task RefreshGameRunningNowAsync(bool notify = false)
    {
        var running = await Task.Run(AzimuthRunning);
        _azimuthRunning = running;
        var now = SavesForceRunning || Playing || running;
        if (now == _gameRunning) return;
        _gameRunning = now;
        if (!notify) return;
        if (On(SavesScreen.Home)) FillSaveCards();
        ChangedSaves();
    }

    /// <summary>
    /// The two cards of the home, rebuilt from <see cref="ProfileRows"/> and this PC's last scan: the HARDCORE profile (missing: its
    /// explainer), then AZIMUTH (OD-240: the account's one identity, nothing to create, choose or delete; missing: its explainer, which picks
    /// the gamertag once) with this PC's Azimuth save of each game.
    /// </summary>
    private void FillSaveCards()
    {
        using var span = Span();
        if (OfflineMode || _session?.Me?.Players is null) { SaveCards.Clear(); ChangedSaves(); return; }
        // owner 2026-11-10: no process list or folder copy on the UI thread here; both come from the last off-thread scan
        _gameRunning = SavesForceRunning || Playing || _azimuthRunning;
        KeepOriginalInBackground();
        var cards = new List<SaveCard>();
        var hardcore = ProfileRows.Where(r => r.Card?.Player.Mode == "hardcore").ToList();
        if (hardcore.Count == 0) cards.Add(HardcoreExplainer());
        foreach (var row in hardcore) cards.Add(CardOf(row));
        cards.Add(AzimuthCard());
        // owner 2026-11-10: in place; a card that did not change keeps its drawing (re-adding the four cards was ~80 ms of layout, on any
        // page), and a game switch that changes them all draws one per frame
        _saveCardsWanted = cards;
        SyncSaveCards(cards);
        // owner 2026-11-08 (image 7): the open ⋯ floats over the page; its owner (a card or a game row) and items come from the rebuilt cards
        OpenMenu = cards.Where(c => c.MenuOpen).Select(c => new FloatingMenuView(c, c.Menu)).Concat(
            cards.SelectMany(c => c.Games).Where(g => g.MenuOpen).Select(g => new FloatingMenuView(g, g.Menu))).FirstOrDefault();
        if (OpenMenu is null) _menu.Close();
        Changed(nameof(OpenMenu));
        Changed(nameof(ShowFloatingMenu));
        Changed(nameof(FloatingMenuIsCard));
        Changed(nameof(FloatingMenuIsRow));
        ChangedSaves();
    }

    private List<SaveCard>? _saveCardsWanted;

    private void SyncSaveCards(List<SaveCard> cards) =>
        ListSync.Sync(SaveCards, cards, SameSaveCard, SameSaveCardKey, Later(() => { if (ReferenceEquals(_saveCardsWanted, cards)) SyncSaveCards(cards); }), budget: 1);

    /// <summary>Harness seam: the cards a fill still draws a few per frame, all at once now.</summary>
    internal void SettleSaveCards()
    {
        if (_saveCardsWanted is { } cards) ListSync.Sync(SaveCards, cards, SameSaveCard, SameSaveCardKey);
    }

    /// <summary>The open ⋯ menu, floating (null: none).</summary>
    public FloatingMenuView? OpenMenu { get; private set; }

    public bool ShowFloatingMenu => OpenMenu is not null && _menu.OpenKey is not null && ShowSavesHome && !_busyState.IsBusy;

    public bool FloatingMenuIsCard => OpenMenu?.Owner is SaveCard;

    public bool FloatingMenuIsRow => OpenMenu?.Owner is ProfileGameRow;

    /// <summary>An outside click or Esc: the open ⋯ closes.</summary>
    public void CloseMenu()
    {
        if (!_menu.Close()) return;
        FillSaveCards();
    }

    private string ModeWord(SaveSlot slot) => S[slot == SaveSlot.Hardcore ? "sm.hardcore" : slot == SaveSlot.Noob ? "sm.noob" : "sm.azimuth"];

    private SaveCard HardcoreExplainer()
    {
        var offered = CurrentGame.HardcoreCareers && CurrentGame.FullFeatures;
        return new SaveCard(SaveSlot.Hardcore, ModeWord(SaveSlot.Hardcore))
        {
            ExplainTitle = S["sm.explain.hc_title"],
            ExplainBody = S["sm.explain.hc_body"],
            CreateLabel = S.Format("sm.explain.create", ("mode", ModeWord(SaveSlot.Hardcore))),
            CreateEnabled = offered,
            CreateNote = offered ? string.Empty : S.Format("sm.no_hardcore_hint", ("game", GameLabel)),
        };
    }

    private SaveCard CardOf(ProfileRow row)
    {
        var card = row.Card!;
        var selected = _gate.ProfileKey is { } key && key == card.CareerId;
        // owner 2026-11-08: the header (mode, avatar, gamertag) and the profile's own ⋯; every game's career is a row under it
        return new SaveCard(SaveSlot.Hardcore, ModeWord(SaveSlot.Hardcore))
        {
            Row = row,
            Hint = S["sm.hint_hardcore"],
            Selected = selected,
            MenuOpen = _menu.IsOpen(card.Player.ProfileId),
            Menu = ProfileMenuOf(row),
            Games = GameRowsOf(row),
            Note = card.Duplicate ? S["careers.duplicate"] : string.Empty,
        };
    }

    /// <summary>The AZIMUTH card's ⋯ owner key (it has no profile id of its own on this page).</summary>
    private const string AzimuthCardKey = "azimuth";

    /// <summary>
    /// The AZIMUTH card (the prototype's "the sandbox way"), the HARDCORE card's peer: the identity's gamertag (OD-240, shown not chosen), a ⋯
    /// with Help Sensei, and one row per catalog game with this PC's Azimuth save of it. Without an identity, its explainer, whose button
    /// picks the gamertag once.
    /// </summary>
    private SaveCard AzimuthCard()
    {
        if (AzimuthIdentityName() is not { } name)
            return new SaveCard(SaveSlot.Azimuth, ModeWord(SaveSlot.Azimuth))
            {
                ExplainTitle = S["sm.explain.az_title"],
                ExplainBody = S["sm.explain.az_body"],
                CreateLabel = S["sm.create_azimuth_title"],
                CreateEnabled = true,
            };
        return new SaveCard(SaveSlot.Azimuth, ModeWord(SaveSlot.Azimuth))
        {
            Name = name,
            Hint = S["sm.hint_azimuth"],
            MenuOpen = _menu.IsOpen(AzimuthCardKey),
            Menu = [new MenuAction(ProfileTools.HelpSensei, S["sm.help_link"], true)],
            Games = AzimuthRowsOf(),
        };
    }

    /// <summary>
    /// The AZIMUTH card's rows: per catalog game, this PC's <c>User_9000000000000</c> as the PLAY card reads it (the cached scan, no request,
    /// no disk read past it): its level, credits and when it was modified, every one from this PC's save (OD-240 #6): the level as the game's
    /// <see cref="GameDefinition.LocalLevel"/> row says (a stored level, or the XP through the game's own XP table read from the player's
    /// install, <see cref="LocalLevels"/>); "—" with the reason in the tooltip when it cannot be read, never a guess. No save is the empty
    /// state in the same fields. Each row's ⋯ holds that save's tools (<see cref="AzimuthMenuOf"/>).
    /// </summary>
    private List<ProfileGameRow> AzimuthRowsOf()
    {
        using var span = Span();
        var rows = new List<ProfileGameRow>();
        foreach (var game in GameDefinition.All)
        {
            rows.Add(InGame(game, () =>
            {
                var art = ArtOf(game);
                SaveProfile? save = null;
                var unreadable = false;
                try { save = AzimuthSave(); }
                catch (SaveException) { unreadable = true; }
                // OD-240 #6: every AZIMUTH value is this PC's save's (the level too, through the game's own data), never the server's
                var level = save is null ? null : LocalLevels.Of(game, save, _install?.Root);
                var line = save is null
                    ? RowFields(null, null, S[unreadable ? "sm.row.az_unreadable" : _install is null ? "sm.row.az_no_install" : "sm.row.az_none"])
                    : RowFields(level?.Shown?.ToString("N0", SavesCulture), save.Credits, S.Format("sm.modified", ("when", When(save.LastPlayedUtc))));
                return new ProfileGameRow(AzimuthCardKey, game.Id, art?.Name ?? game.Name, game.ShortName,
                    TileBrush(game.TileAccent, 0xFF), TileBrush(game.TileAccent, 0x2E), line)
                {
                    Azimuth = true,
                    LineTip = level?.Why is { } why ? line + "\n" + S.Format("sm.level_why", ("why", S[why])) : line,
                    Cover = art?.Image,
                    HasCareerData = save is not null,
                    ChipText = S[save is null ? "sm.chip.az_none" : "sm.chip.az_pc"],
                    ChipTip = S["sm.chip.az_tip"],
                    ChipTone = ChipTone.Neutral,
                    MenuOpen = _menu.IsOpen(AzimuthCardKey + "|" + game.Id),
                    Menu = AzimuthMenuOf(game),
                };
            }));
        }
        return rows;
    }

    /// <summary>
    /// An AZIMUTH row's ⋯ (what "Manage saves" and Settings' "Old Noob saves" held, now by the save they act on): Copy from HARDCORE (its
    /// save of the game, onto this one after a verified backup), History, Restore Azimuth's original, and per old Noob save of the game its
    /// copy (after a backup whose path is told) and its export (a cloud one) or its folder (one on this PC). Disabled with the reason when
    /// it cannot run now (the game running, nothing to copy, no original recorded).
    /// </summary>
    private List<MenuAction> AzimuthMenuOf(GameDefinition game)
    {
        var running = _gameRunning;
        var old = OldSaveRows.Where(r => r.Save.Game == game.Id).ToList();
        string? blocked = game.Id == _gate.Game ? null
            : !Games.Any(g => g.Id == game.Id) ? S["play.tile_not_in_install"]
            : Playing ? S["play.session_active"] : null;
        var history = SafeHistory();
        var list = new List<MenuAction>();
        foreach (var id in ProfileTools.AzimuthMenu([.. old.Select(r => r.Save.Key)]))
        {
            var (tool, key) = ProfileTools.Split(id);
            var row = key is null ? null : old.First(r => r.Save.Key == key);
            var action = tool switch
            {
                ProfileTools.UseOtherSave => new MenuAction(id, S["sm.menu.use_other_save"], !running, running ? S["sm.block.GameRunning"] : null),
                ProfileTools.History => new MenuAction(id, S["sm.menu.history"], true),
                ProfileTools.RestoreOriginal => running ? new MenuAction(id, S["sm.restore_original"], false, S["sm.block.GameRunning"])
                    : history?.HasOriginal == true ? new MenuAction(id, S["sm.restore_original"], true)
                    : new MenuAction(id, S["sm.restore_original"], false, S.Format("sm.restore_original_none", ("game", game.Name))),
                ProfileTools.OldCopy => new MenuAction(id, S.Format("export.menu_copy", ("name", row!.Save.Gamertag)), !running, running ? S["sm.block.GameRunning"] : null),
                _ => new MenuAction(id, S.Format("export.menu_export", ("name", row!.Save.Gamertag)), CanExportOldNoob),
            };
            list.Add(blocked is not null ? action with { Enabled = false, Reason = blocked } : action);
        }
        return list;
    }

    /// <summary>The row's fields, the same on both cards: "LV 15  ·  CR 15,858  ·  {last}", an unknown level or number shown "—".</summary>
    private string RowFields(string? level, long? credits, string last) => S.Format("sm.row.fields",
        ("level", level ?? S["sm.unknown"]),
        ("credits", credits?.ToString("N0", SavesCulture) ?? S["sm.unknown"]),
        ("last", last));

    /// <summary>The profile card's ⋯ (owner 2026-11-08): Delete profile (typed gamertag), Rename when the server allows it, Help Sensei.</summary>
    private List<MenuAction> ProfileMenuOf(ProfileRow row)
    {
        var card = row.Card!;
        var list = new List<MenuAction>();
        foreach (var id in ProfileTools.ProfileMenu(RenamableCopy(card.Player) is not null, row.CanDeleteWholeProfile))
        {
            list.Add(id switch
            {
                ProfileTools.DeleteProfile => new MenuAction(id, S["sm.menu.delete_profile"], CanAskDeleteProfile, Danger: true),
                ProfileTools.Rename => new MenuAction(id, S["sm.menu.rename"], Idle),
                _ => new MenuAction(id, S["sm.help_link"], true),
            });
        }
        return list;
    }

    /// <summary>
    /// One row per catalog game (owner 2026-11-08), from the one <c>/me</c> and this PC's files of each game: no request. A game other than
    /// the current one is read inside <see cref="InGame"/>; its actions switch PLAY's game to it first (one game at a time in the save tools).
    /// </summary>
    private List<ProfileGameRow> GameRowsOf(ProfileRow row)
    {
        using var span = Span();
        var rows = new List<ProfileGameRow>();
        if (_session?.Me is not { Players: not null } board) return rows;
        var profileId = row.Card!.Player.ProfileId;
        foreach (var game in GameDefinition.All)
        {
            rows.Add(InGame(game, () =>
            {
                var card = game.Id == _gate.Game ? row.Card
                    : ProfileGameRows.Build(board, profileId, [game], _ => BoardFlagsNow(board)).Single().Card;
                var facts = new ProfileGameFacts(game, card);
                var chip = card is null ? SaveChipKind.None : ChipOf(card);
                var art = ArtOf(game);
                // owner 2026-11-08 (image 7): no cloud progress but a save on this PC says what is known (its credits when readable)
                var local = !facts.HasCareerData && card is not null && card.State != CareerState.NotOffered ? LocalSaveOf(PlayedFolderXuid(card.Player)) : null;
                return new ProfileGameRow(profileId, game.Id, art?.Name ?? game.Name, game.ShortName,
                    TileBrush(game.TileAccent, 0xFF), TileBrush(game.TileAccent, 0x2E), GameRowLine(facts, local))
                {
                    Cover = art?.Image,
                    HasCareerData = facts.HasCareerData,
                    ChipText = ChipShort(chip, card?.Career),
                    ChipTip = ChipText(chip, card?.Career),
                    ChipTone = SaveChips.Tone(chip),
                    MenuOpen = _menu.IsOpen(profileId + "|" + game.Id),
                    Menu = CareerMenuOf(facts, chip),
                };
            }));
        }
        return rows;
    }

    /// <summary>
    /// A HARDCORE row's fields (the AZIMUTH row's too, <see cref="RowFields"/>): the server's level and credits and when the cloud save was
    /// made; a save on this PC not uploaded yet gives its credits and time (no level: only the server derives it, CR-S81); otherwise the
    /// empty state in the same place ("no career data yet", or the game has no HARDCORE careers).
    /// </summary>
    private string GameRowLine(ProfileGameFacts facts, SaveProfile? local = null)
    {
        if (facts.Card?.State == CareerState.NotOffered) return RowFields(null, null, S.Format("sm.no_hardcore", ("game", facts.Game.Name)));
        if (facts.HasCareerData && facts.Card!.Career!.Progress is { } p)
        {
            var at = facts.Card.Career.Cloud?.SavedAt ?? p.SyncedAt;
            return RowFields(facts.Game.ShownLevel(p.Level)?.ToString("N0", SavesCulture), p.Credits, at is null ? S["sm.row.cloud"] : S.Format("sm.modified", ("when", When(at))));
        }
        if (local is not null)
            return RowFields(null, local.Credits, S.Format(facts.Game.CloudSaves ? "sm.row.local_when" : "sm.modified", ("when", When(local.LastPlayedUtc))));
        return RowFields(null, null, S["sm.row.no_data"]);
    }

    /// <summary>The chip's short words on a row (the full sentence is its tooltip, <see cref="ProfileGameRow.ChipTip"/>).</summary>
    private string ChipShort(SaveChipKind kind, Career? career) => kind switch
    {
        SaveChipKind.NotSyncedYet => S["sm.chip.short.not_synced"],
        SaveChipKind.NotCompared => S["sm.chip.short.not_compared"],
        SaveChipKind.NewCareer => S["sm.chip.short.new"],
        SaveChipKind.SeedPending => S["sm.chip.short.seed"],
        _ => ChipText(kind, career),
    };

    /// <summary>A game row's ⋯ (owner 2026-11-08): that game's career actions, each opening the existing screen with the game set.</summary>
    private List<MenuAction> CareerMenuOf(ProfileGameFacts facts, SaveChipKind chip)
    {
        var list = new List<MenuAction>();
        var card = facts.Card;
        var running = _gameRunning;
        var game = facts.Game;
        // the save tools act on one game: another game's row switches PLAY's game first, which needs it offered and no session running
        var otherGame = game.Id != _gate.Game;
        string? blocked = !otherGame ? null
            : !Games.Any(g => g.Id == game.Id) ? S["play.tile_not_in_install"]
            : Playing ? S["play.session_active"] : null;
        var canDeleteCareer = card?.CareerRow is not null;
        foreach (var id in ProfileTools.CareerMenu(facts.Started, SaveChips.CanDownload(chip) && game.CloudSaves, canDeleteCareer))
        {
            var action = id switch
            {
                // OD-239 BUG A: a HARDCORE row copies its own save, named so (only HARDCORE profiles have rows, OD-240)
                ProfileTools.CopyTo => LocalSaveOf(card!.Player.Xuid) is not null
                    ? new MenuAction(id, S["sm.menu.copy_azimuth"], !running, running ? S["sm.block.GameRunning"] : null)
                    : new MenuAction(id, S["sm.menu.copy_azimuth"], false, S["sm.block.NoSource"]),
                ProfileTools.History => new MenuAction(id, S["sm.menu.history"], true),
                ProfileTools.Resolve => SaveChips.NeedsResolve(chip)
                    ? new MenuAction(id, S["sm.resolve"], true) : new MenuAction(id, S["sm.resolve"], false, S["sm.menu.resolve_nothing"]),
                ProfileTools.Download => new MenuAction(id, S["sm.menu.download"], !running, running ? S["sm.block.GameRunning"] : null),
                _ => new MenuAction(id, S.Format("sm.menu.delete_career", ("game", game.Name)), CanAskDeleteProfile, Danger: true),
            };
            list.Add(blocked is not null ? action with { Enabled = false, Reason = blocked } : action);
        }
        // OD-133 (owner 2026-11-12: off PLAY): the chosen HARDCORE profile's row of PLAY's game, while a career a fresh start moved aside is here
        if (!otherGame && card?.Player.ProfileId == _gate.ProfileKey && ShowCareerRestoreEntry)
        {
            var at = list.FindIndex(a => a.Id == ProfileTools.DeleteCareer);
            list.Insert(at >= 0 ? at : list.Count,
                new MenuAction(ProfileTools.RestoreCareer, S["restore.entry"], CanRestoreCareer, CareerRestoreNote is { Length: > 0 } why ? why : null));
        }
        return list;
    }

    /// <summary>The chip of a card, from this PC's files and sync state only (no request).</summary>
    private SaveChipKind ChipOf(ProfileRow row) => ChipOf(row.Card!);

    /// <summary>
    /// "Keep this PC" choices made on Resolve, per (game, XUID), in memory only (no sync-state change): the chip shows "uploads at the
    /// next Play" and that Play commits this PC's save without asking again. Read by a Play's save step off the UI thread.
    /// </summary>
    private readonly System.Collections.Concurrent.ConcurrentDictionary<(string Game, ulong Xuid), KeptThisPc> _keptThisPc = new();

    private void ForgetKeptThisPc(string game, ulong xuid)
    {
        if (_keptThisPc.TryRemove((game, xuid), out _)) Log($"saves: keep this PC of {game} {xuid} forgotten");
    }

    /// <summary>The recorded choice for this cloud revision and this PC's save; a stale one (either side moved) is dropped.</summary>
    private KeptThisPc? KeptThisPcFor(string game, ulong xuid, ulong? cloudRevision, string? localContent)
    {
        if (!_keptThisPc.TryGetValue((game, xuid), out var kept)) return null;
        if (kept.Matches(cloudRevision, localContent)) return kept;
        _keptThisPc.TryRemove(new KeyValuePair<(string, ulong), KeptThisPc>((game, xuid), kept));
        Log($"saves: keep this PC of {game} {xuid} is stale (the cloud or this PC moved), dropped");
        return null;
    }

    /// <summary>The chip of a career of the current game (or of the row's game being read, <see cref="InGame"/>).</summary>
    private SaveChipKind ChipOf(BoardCard card)
    {
        if (card.State != CareerState.Started) return SaveChipKind.None;
        // OD-239: an Azimuth career has no cloud save: its chip is this PC's only (never a sync, a resolve or a download)
        var cloudSaves = CurrentGame.CloudSaves && card.Player.Mode == "hardcore";
        string? content = null; SyncMark? mark = null; var pending = false; KeptThisPc? kept = null;
        if (ulong.TryParse(card.Player.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid) && SavesRootNow is { } root)
        {
            try
            {
                // OD-239 item C: an Azimuth profile's folder is Azimuth's own save, never its old User_<XUID>
                if (NewestSealedOf(CloudSaveFiles.ProfileDir(root, cloudSaves ? xuid : ulong.Parse(PlayedFolderXuid(card.Player), CultureInfo.InvariantCulture))) is { } newest)
                    content = cloudSaves ? ContentDigestOf(CurrentGame, newest) : "local";
            }
            catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException) { content = "unreadable"; }
            if (cloudSaves && CloudSaveClient() is { } client && client.Saves.State(xuid) is { } state)
            {
                mark = state.Mark;
                pending = state.PendingUpload is not null && state.PendingUpload.Kind != CommitKinds.SeedFromProfile;
            }
            if (cloudSaves)
                kept = KeptThisPcFor(CurrentGame.Id, xuid, card.Career?.Cloud?.Revision is long r and > 0 ? (ulong)r : null, content);
        }
        return SaveChips.Of(new SaveChipInput(card.State, cloudSaves, card.Career?.Cloud?.Revision, content, mark, pending, card.SeedPending, kept));
    }

    private string ChipText(SaveChipKind kind, Career? career) => kind switch
    {
        SaveChipKind.None => string.Empty,
        SaveChipKind.Synced => S.Format("sm.chip.synced", ("ago", Ago(career?.Cloud?.SavedAt ?? career?.Progress?.SyncedAt))),
        _ => S["sm.chip." + kind switch
        {
            SaveChipKind.ThisPcNewer => "pc_newer",
            SaveChipKind.CloudNewer => "cloud_newer",
            SaveChipKind.BothChanged => "both",
            SaveChipKind.NotSyncedYet => "not_synced",
            SaveChipKind.CloudOnly => "cloud_only",
            SaveChipKind.NotCompared => "not_compared",
            SaveChipKind.LocalOnly => "local_only",
            SaveChipKind.NewCareer => "new",
            SaveChipKind.SeedPending => "seed",
            _ => "waiting",
        }],
    };

    /// <summary>"just now", "5 min ago", "3 h ago", "2 days ago", then the date.</summary>
    internal string Ago(string? at)
    {
        if (at is null || !DateTimeOffset.TryParse(at, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var when)) return S["sm.ago.unknown"];
        var span = (SavesNow ?? DateTimeOffset.UtcNow) - when;
        var c = SavesCulture;
        return span.TotalMinutes < 1 ? S["sm.ago.now"]
            : span.TotalHours < 1 ? S.Format("sm.ago.min", ("n", ((int)span.TotalMinutes).ToString(c)))
            : span.TotalDays < 1 ? S.Format("sm.ago.hours", ("n", ((int)span.TotalHours).ToString(c)))
            : span.TotalDays < 7 ? S.Format("sm.ago.days", ("n", ((int)span.TotalDays).ToString(c)))
            : when.ToLocalTime().ToString("d MMM", c);
    }

    /// <summary>Harness seam: a fixed "now" for the chips' "2 min ago".</summary>
    internal DateTimeOffset? SavesNow { get; set; }

    private string When(DateTime utc) => utc.ToLocalTime().ToString(SavesCulture.Name == "pt-BR" ? "d MMM, HH:mm" : "MMM d, HH:mm", SavesCulture);

    private string When(string? at) => at is not null && DateTimeOffset.TryParse(at, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var w)
        ? When(w.UtcDateTime) : S["sm.unknown"];

    // ------------------------------------------------------------------ this PC's saves

    private string? SavesRootNow => _install is null ? null : AzimuthSaves.SavesRoot(_install.Root, CurrentGame);

    private LocalHistory History() => new(_stateRoot, CurrentGame, Version);

    /// <summary>
    /// OD-239 item C: the folder a profile PLAYS (decimal XUID): an Azimuth (wire <c>noob</c>) profile plays Azimuth's own
    /// <c>User_9000000000000</c>; a Hardcore one its own XUID's folder. Its own old folder is an old save only (ShellViewModel.OldSaves.cs).
    /// </summary>
    private static string PlayedFolderXuid(Player player) =>
        player.Mode == "hardcore" ? player.Xuid : AzimuthSaves.OfflineXuid.ToString(CultureInfo.InvariantCulture);

    /// <summary>The newest save of <paramref name="xuid"/>'s folder on this PC (any game layout Azimuth writes), or null.</summary>
    private SaveProfile? LocalSaveOf(string xuid)
    {
        if (SavesRootNow is not { } root || !ulong.TryParse(xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var x)) return null;
        try { return SavesList(root).FirstOrDefault(s => s.Xuid == x); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    /// <summary>Azimuth's own player on this PC (<c>User_9000000000000</c>): the save PLAY AZIMUTH plays, online or offline.</summary>
    private SaveProfile? AzimuthSave()
    {
        if (SavesRootNow is not { } root) return null;
        try { return SavesList(root).FirstOrDefault(s => s.IsOffline); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    /// <summary>§7.2: the title's saves as first found, once per folder and game, on a worker (owner 2026-11-10: the cards never wait for the copy).</summary>
    private void KeepOriginalInBackground()
    {
        if (SavesRootNow is not { } savesRoot || _install?.Root is not { } root || !_originalKept.Add(root + "|" + CurrentGame.Id)) return;
        var history = History();
        var game = CurrentGame;
        _ = Task.Run(() => KeepOriginal(history, savesRoot, game));
    }

    /// <summary>
    /// §7.1 "Every write first archives": <paramref name="folder"/> into a new history entry, verified. False (and the reason shown) when it
    /// could not be kept: the caller then writes nothing. Owner 2026-11-10 (the window never freezes): the copy and its verification (a
    /// whole saves folder, the first time its original too) run on a worker; the caller awaits it under its busy overlay.
    /// </summary>
    private async Task<bool> ArchiveFirstAsync(string action, string? folder, string reasonKey, (string, string)[] args, string? from = null, string? to = null, ProfileRow? row = null)
    {
        if (SavesRootNow is not { } root) return false;
        try
        {
            var history = History();
            var reason = new HistoryReason(StringsIn("en").Format(reasonKey, args), StringsIn("pt-BR").Format(reasonKey, args));
            var profile = row is null ? null : new HistoryProfile(row.Profile.Xuid, row.Gamertag, row.Profile.Mode);
            var slug = from is null ? null : from + " to " + to;
            var item = await Task.Run(() =>
            {
                history.EnsureOriginal(root, DateTime.UtcNow);
                return history.Archive(action, folder, reason, DateTime.UtcNow, from, to, profile, slug: slug);
            });
            Log($"saves: history {item.Name} ({item.Entry.Files.Count} files)");
            return true;
        }
        catch (Exception e) when (e is HistoryException or IOException or UnauthorizedAccessException)
        {
            Log("saves: history not written, action stopped: " + e.Message);
            Message = S["sm.history_failed"];
            return false;
        }
    }

    private readonly Dictionary<string, Infinity.Core.Strings> _stringsByLocale = new(StringComparer.Ordinal);

    /// <summary>The same strings in another language (entry.json keeps the reason in English and Portuguese).</summary>
    private Infinity.Core.Strings StringsIn(string locale)
    {
        if (S.Locale == locale) return S;
        if (_stringsByLocale.TryGetValue(locale, out var cached)) return cached;
        Infinity.Core.Strings loaded;
        try { loaded = Infinity.Core.Strings.Load(Path.Combine(AppContext.BaseDirectory, "i18n"), locale).With(Path.Combine(AppContext.BaseDirectory, "i18n-win")); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or System.Text.Json.JsonException) { loaded = S; }
        return _stringsByLocale[locale] = loaded;
    }

    private string ProfileFolder(ProfileRow row) => SavesRootNow is { } root && ulong.TryParse(row.Profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var x)
        ? CloudSaveFiles.ProfileDir(root, x) : string.Empty;

    private string AzimuthFolder => SavesRootNow is { } root ? CloudSaveFiles.ProfileDir(root, AzimuthSaves.OfflineXuid) : string.Empty;

    // ------------------------------------------------------------------ card actions

    public void ToggleSavesMenu(SaveCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (_busyState.IsBusy) return; // review 3.1.1: no menu opens under a lock or a load
        var id = card.IsAzimuth && card.IsProfile ? AzimuthCardKey : card.Row?.Card?.Player.ProfileId;
        if (id is not null) _menu.Toggle(id); else _menu.Close();
        FillSaveCards();
    }

    /// <summary>A game row's ⋯ (owner 2026-11-08).</summary>
    public void ToggleGameRowMenu(ProfileGameRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (_busyState.IsBusy) return;
        _menu.Toggle(row.MenuKey);
        FillSaveCards();
    }

    /// <summary>A card's ⋯: profile-level actions only (owner 2026-11-08); the AZIMUTH card's is Help Sensei alone.</summary>
    public void SavesMenu(SaveCard card, MenuAction action)
    {
        ArgumentNullException.ThrowIfNull(card);
        ArgumentNullException.ThrowIfNull(action);
        if (!action.Enabled) { if (action.Reason is { } why) Message = why; return; }
        if (card.Row is not { } row)
        {
            _menu.Close();
            if (card.IsAzimuth && action.Id == ProfileTools.HelpSensei) OpenSavesHelp();
            else FillSaveCards();
            return;
        }
        _menu.Close();
        switch (action.Id)
        {
            case ProfileTools.HelpSensei:
                OpenSavesHelp();
                break;
            case ProfileTools.Rename:
                OpenRename(row.Card?.Player);
                break;
            case ProfileTools.DeleteProfile:
                AskDeleteWholeProfile(row.Profile);
                break;
        }
        ChangedSaves();
    }

    /// <summary>
    /// A game row's career action (owner 2026-11-08): the row's game becomes the current one (PLAY's selection follows, as a tile would), then
    /// the existing screen opens for this profile's career of it; the breadcrumb names the game.
    /// </summary>
    public async Task GameRowMenuAsync(ProfileGameRow gameRow, MenuAction action)
    {
        ArgumentNullException.ThrowIfNull(gameRow);
        ArgumentNullException.ThrowIfNull(action);
        if (!action.Enabled) { if (action.Reason is { } why) Message = why; return; }
        _menu.Close();
        if (gameRow.GameId != _gate.Game)
        {
            if (Games.FirstOrDefault(g => g.Id == gameRow.GameId) is not { } info || Playing)
            {
                Message = Playing ? S["play.session_active"] : S["play.tile_not_in_install"];
                FillSaveCards();
                return;
            }
            ChooseGame(info);
            Log($"saves: {gameRow.GameId} chosen from a Profiles row");
            Changed(string.Empty);
        }
        if (gameRow.Azimuth) { await AzimuthRowMenuAsync(action); ChangedSaves(); return; }
        var row = ProfileRows.FirstOrDefault(r => r.Card?.Player.ProfileId == gameRow.ProfileId);
        if (row is null) { FillSaveCards(); return; }
        // OD-240: only a HARDCORE profile has rows (an AZIMUTH one is never offered here)
        if (!row.Hardcore) { FillSaveCards(); return; }
        var slot = SlotOf(row);
        switch (action.Id)
        {
            case ProfileTools.CopyTo:
                OpenCopyFromProfile(slot, row);
                break;
            case ProfileTools.History:
                await OpenHistoryAsync(slot);
                break;
            case ProfileTools.Resolve:
                OpenResolve(row);
                break;
            case ProfileTools.Download:
                await DownloadCareerAsync(row);
                FillSaveCards();
                break;
            case ProfileTools.DeleteCareer:
                AskDeleteProfile(row.Profile);
                break;
            case ProfileTools.RestoreCareer:
                await OpenCareerRestoreAsync();
                break;
        }
        ChangedSaves();
    }

    /// <summary>
    /// An AZIMUTH row's tool, its game already the current one: each opens the screen or runs the action that "Manage saves" and Settings'
    /// "Old Noob saves" ran, unchanged (every write backs the Azimuth save up first, verified, and is refused while the game runs).
    /// </summary>
    private async Task AzimuthRowMenuAsync(MenuAction action)
    {
        var (tool, key) = ProfileTools.Split(action.Id);
        var old = key is null ? null : OldSaveRows.FirstOrDefault(r => r.Save.Key == key && r.Save.Game == CurrentGame.Id);
        switch (tool)
        {
            case ProfileTools.UseOtherSave:
                await OpenOtherSaveAsync();
                break;
            case ProfileTools.History:
                await OpenHistoryAsync(SaveSlot.Azimuth);
                break;
            case ProfileTools.RestoreOriginal:
                await OpenRestoreOriginalAsync();
                break;
            case ProfileTools.OldCopy when old is not null:
                AskMoveOldSave(old); // owner 2026-11-12: the copy's centred confirmation first
                break;
            case ProfileTools.OldExport when old is not null:
                ExportOldSave(old);
                break;
            default:
                FillSaveCards(); // the old save went away meanwhile
                break;
        }
    }

    /// <summary>
    /// An explainer's button: "Create a HARDCORE profile" opens the new-profile form; AZIMUTH's "Pick your Azimuth gamertag" the same form for
    /// the account's one identity (OD-240: picked once).
    /// </summary>
    public void OpenCreateProfile(SaveCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (!card.CreateEnabled || !card.IsExplainer) return;
        _createMode = card.IsAzimuth ? "noob" : "hardcore";
        NewHardcore = !card.IsAzimuth;
        SavesGo(SavesScreen.Create);
    }

    /// <summary>
    /// The form's words: a HARDCORE profile is created; the AZIMUTH identity's gamertag is picked once (OD-240: PLAY AZIMUTH opens this only
    /// while the account has none).
    /// </summary>
    public string CreateTitle => _createMode == "hardcore" ? S.Format("sm.create_title", ("mode", ModeWord(SaveSlot.Hardcore))) : S["sm.create_azimuth_title"];

    public string CreateSub => S[_createMode == "hardcore" ? "sm.create_sub" : "sm.create_azimuth_sub"];

    public Brush CreateAccent => SlotBrushes.Accent(_createMode == "hardcore" ? SaveSlot.Hardcore : SaveSlot.Noob);

    // ------------------------------------------------------------------ Start (§2.6, OD-214)

    public string StartSub => S["sm.start_sub"];

    /// <summary>OD-240: only a HARDCORE career opens the Start choice (an AZIMUTH one is made behind the scenes at PLAY AZIMUTH).</summary>
    public string StartFreshHint => S.Format("sm.start.fresh_hint", ("game", GameLabel));

    /// <summary>
    /// "From my AZIMUTH profile" (§2.6): a HARDCORE card starts from the Azimuth career through the CR-S52 check. Shown on a HARDCORE card
    /// only (OD-239: an Azimuth career starts fresh only); disabled with the reason when not possible.
    /// </summary>
    public bool ShowStartFromProfile => StartCard is not null && CareerStartHardcore;

    /// <summary>"From this PC's save" is a HARDCORE card's disabled choice with its B2 reason; an Azimuth card has nothing else to offer.</summary>
    public bool ShowStartFromPc => StartCard is not null && CareerStartHardcore;

    public string StartFromProfileLabel => S.Format("sm.start.from_profile", ("mode", ModeWord(CareerStartHardcore ? SaveSlot.Noob : SaveSlot.Hardcore)));

    public string StartFromProfileHint => !CareerStartHardcore ? string.Empty
        : CareerStartFromExisting ? CareerStartExistingHint
        : !CurrentGame.FullFeatures ? S.Format("sm.start.cloudless", ("game", GameLabel))
        : S.Format("sm.start.from_profile_off", ("mode", ModeWord(SaveSlot.Noob)), ("game", GameLabel));

    public bool StartFromProfileEnabled => CareerStartHardcore && CareerStartFromExisting && Idle;

    public string StartFromPcHint => S["sm.start.from_pc_hc"];

    public Brush StartAccent => SlotBrushes.Accent(CareerStartHardcore ? SaveSlot.Hardcore : SaveSlot.Noob);

    // ------------------------------------------------------------------ Resolve (§4)

    public CompareSide? ResolveCloud { get; private set; }
    public CompareSide? ResolvePc { get; private set; }
    public string ResolveTitle => _subject is null ? string.Empty : S.Format("sm.resolve_title", ("mode", ModeWord(SlotOf(_subject))));
    public bool ResolveHardcore => _subject?.Hardcore == true;

    private static SaveSlot SlotOf(ProfileRow row) => row.Hardcore ? SaveSlot.Hardcore : SaveSlot.Noob;

    private void OpenResolve(ProfileRow row)
    {
        _subject = row;
        var card = row.Card!;
        var chip = ChipOf(row);
        var local = LocalSaveOf(row.Profile.Xuid);
        DateTimeOffset? cloudAt = DateTimeOffset.TryParse(card.Career?.Cloud?.SavedAt, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var ca) ? ca : null;
        DateTimeOffset? pcAt = local is null ? null : new DateTimeOffset(local.LastPlayedUtc, TimeSpan.Zero);
        _resolvePick = SaveChips.Recommend(chip, cloudAt, pcAt);
        var c = SavesCulture;
        var p = card.Career?.Progress;
        string N(long? v) => v is { } x ? x.ToString("N0", c) : S["sm.unknown"];
        ResolveCloud = new CompareSide(S["sm.side.cloud"], S[_resolvePick == ResolvePick.Cloud ? "sm.newer" : "sm.older"], _resolvePick == ResolvePick.Cloud,
        [
            new(S["sm.kv.level"], N(CurrentGame.ShownLevel(p?.Level))), new(S["sm.kv.credits"], N(p?.Credits)), new(S["sm.kv.cars"], N(p?.Cars)),
            new(S["sm.kv.play"], p?.PlaySeconds is { } s ? S.Format("sm.stats_hours", ("hours", (s / 3600).ToString(c))) : S["sm.unknown"]),
            new(S["sm.kv.modified"], When(card.Career?.Cloud?.SavedAt)),
            new(S["sm.kv.revision"], card.Career?.Cloud?.Revision.ToString(c) ?? S["sm.unknown"]),
        ])
        { ActionLabel = Recommended(S["sm.keep_cloud"], _resolvePick == ResolvePick.Cloud), Recommended = _resolvePick == ResolvePick.Cloud };
        ResolvePc = new CompareSide(S["sm.side.pc"], S[_resolvePick == ResolvePick.ThisPc ? "sm.newer" : "sm.older"], _resolvePick == ResolvePick.ThisPc,
        [
            new(S["sm.kv.level"], S["sm.unknown"]), new(S["sm.kv.credits"], N(local?.Credits)), new(S["sm.kv.cars"], S["sm.unknown"]),
            new(S["sm.kv.play"], S["sm.unknown"]),
            new(S["sm.kv.modified"], local is null ? S["sm.unknown"] : When(local.LastPlayedUtc)),
            new(S["sm.kv.revision"], local is null ? S["sm.unknown"] : S.Format("sm.local_rev", ("n", local.Revision.Number.ToString(c)))),
        ])
        { ActionLabel = Recommended(S["sm.keep_pc"], _resolvePick == ResolvePick.ThisPc), Recommended = _resolvePick == ResolvePick.ThisPc };
        SavesGo(SavesScreen.Resolve);
    }

    private string Recommended(string label, bool recommended) => recommended ? S.Format("sm.recommended", ("label", label)) : label;

    /// <summary>"Keep cloud": the cloud's newest revision onto this PC (this PC's save archived in History first, then by the install).</summary>
    public async Task ResolveKeepCloudAsync()
    {
        if (_subject is not { } row || _session is not { } session || _install is null || CloudSaveClient() is not { } client || ParseXuid(row.Profile) is not { } xuid) return;
        await RefreshGameRunningNowAsync();
        if (_gameRunning) { Message = S["saves.game_running"]; return; }
        ForgetKeptThisPc(CurrentGame.Id, xuid); // the player now picks the cloud
        await Run(S.Format("busy.downloading", ("mode", BusyMode(row.Profile.Mode))), async () =>
        {
            SavesChanged(); // owner 2026-11-08: this write changes the cloud revisions, read again when next shown
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            var (head, failure) = await client.Saves.HeadAsync(access, xuid);
            if (head is null || failure is not null || head.Revision == 0) { Message = S["careers.download_failed"]; return; }
            var root = AzimuthSaves.SavesRoot(_install.Root, CurrentGame);
            var decision = client.Saves.Decide(root, xuid, head);
            if (decision.Failure is not null) { Message = S["careers.download_failed"]; return; }
            if (!await ArchiveFirstAsync("resolve", ProfileFolder(row), "sm.reason.resolve_cloud", [("mode", ModeWordIn(row))], "this-pc", "cloud", row)) return;
            var outcome = await client.Saves.InstallHeadAsync(access, root, xuid, head, decision.LocalContent, "before-resolve");
            Log($"saves: resolve {row.Profile.Game} {row.Profile.ProfileId} keep cloud r{head.Revision} → {(outcome.Failure is null ? "installed" : outcome.Failure.Code)}");
            if (outcome.Failure is not null) { Message = S["careers.download_failed"]; return; }
            Done(S["sm.kept_cloud"]);
        });
    }

    /// <summary>
    /// "Keep this PC": nothing is written now; the choice is recorded for this cloud revision and this PC's save, so the card no longer
    /// asks to resolve and the next Play sends this PC's save (the session's upload, with the HARDCORE check) without asking again.
    /// </summary>
    public void ResolveKeepPc()
    {
        if (_subject is not { } row) return;
        if (CurrentGame.CloudSaves && row.Card?.Career?.Cloud?.Revision is long revision and > 0 && SavesRootNow is { } root
            && ulong.TryParse(row.Profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid))
        {
            string? content = null;
            try
            {
                if (NewestSealedOf(CloudSaveFiles.ProfileDir(root, xuid)) is { } newest)
                    content = SaveReconcile.NormalizeDigest(ContentDigestOf(CurrentGame, newest));
            }
            catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException) { }
            if (content is not null) _keptThisPc[(CurrentGame.Id, xuid)] = new KeptThisPc((ulong)revision, content);
            else Log($"saves: resolve {row.Profile.Game} {row.Profile.ProfileId} keep this PC not recorded (this PC's save unreadable)");
        }
        Log($"saves: resolve {row.Profile.Game} {row.Profile.ProfileId} keep this PC (uploaded at the next Play)");
        PickProfileRow(row);
        Done(S["sm.kept_pc"]); // the prototype's Keep this PC lands on Done, as Keep cloud does
    }

    private static string ModeWordIn(ProfileRow row) => row.Hardcore ? "HARDCORE" : "AZIMUTH";

    private string Size(long bytes) => bytes >= 1 << 20 ? (bytes / 1048576.0).ToString("0.0", SavesCulture) + " MB"
        : bytes >= 1024 ? (bytes / 1024.0).ToString("0", SavesCulture) + " KB" : bytes.ToString(SavesCulture) + " B";

    private LocalHistory? SafeHistory()
    {
        try { return History(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    // ------------------------------------------------------------------ Copy (§2.1: Compare → Check → Confirm; only into Azimuth today)

    public CompareSide? CopyFromSide { get; private set; }
    public CompareSide? CopyToSide { get; private set; }
    public string CopyTitle { get; private set; } = string.Empty;
    public string CopySub { get; private set; } = string.Empty;
    public string CopyNote { get; private set; } = string.Empty;
    public bool CanConfirmCopy => _copyAsk is not null && !_gameRunning && Idle;
    public string CopyBlockedText => _gameRunning ? GameRunningText : string.Empty;
    public bool ShowCopyBlocked => _gameRunning && _copyAsk is not null;

    /// <summary>
    /// Owner 2026-11-12 (first-hand, WINDOWS chat): every confirmation that copies, restores or replaces a save is a centred popup (title, what
    /// is replaced, the backup line, Confirm / Cancel), never a page step or an inline panel: a HARDCORE save, a history entry, a cloud revision
    /// or Azimuth's original into the Azimuth save (Copy and replace), "Use another save on this PC…" (a listed save or a picked folder) and an
    /// old Noob cloud save's "Copy into my Azimuth save". Nothing is written before Confirm; the write then runs under the busy overlay.
    /// </summary>
    private sealed record CopyAsk(string ConfirmLabel, Func<Task> Run);

    private CopyAsk? _copyAsk;

    /// <summary>The save copy's confirmation popup (it waits behind the prompts of <see cref="PromptOpen"/>).</summary>
    public bool ShowCopyConfirm => _copyAsk is not null && !PromptOpen;

    public string CopyConfirmLabel => _copyAsk?.ConfirmLabel ?? S["sm.copy_confirm"];

    /// <summary>Opens the confirmation popup: <paramref name="from"/> → the Azimuth save (<paramref name="to"/>), with its backup line.</summary>
    private void AskCopy(string title, string sub, CompareSide from, CompareSide to, string note, string confirm, Func<Task> run)
    {
        RefreshGameRunning();
        _menu.Close();
        CopyTitle = title; CopySub = sub; CopyFromSide = from; CopyToSide = to; CopyNote = note;
        _copyAsk = new CopyAsk(confirm, run);
        Log("saves: copy confirmation shown: " + title);
        ChangedSaves();
    }

    /// <summary>Cancel: the popup closes, nothing written.</summary>
    public void CancelCopy()
    {
        if (_copyAsk is null) return;
        _copyAsk = null; _copy = null;
        Log("saves: copy confirmation cancelled");
        ChangedSaves();
    }

    /// <summary>The Azimuth save of the current game as the "Replaced" side reads it (credits, modified, revision).</summary>
    private List<CompareLine> AzimuthSideLines()
    {
        var c = SavesCulture;
        var az = AzimuthSave();
        return
        [
            new(S["sm.kv.credits"], az?.Credits?.ToString("N0", c) ?? S["sm.unknown"]),
            new(S["sm.kv.modified"], az is null ? S["sm.slot.no_save"] : When(az.LastPlayedUtc)),
            new(S["sm.kv.revision"], az is null ? S["sm.unknown"] : S.Format("sm.local_rev", ("n", az.Revision.Number.ToString(c)))),
        ];
    }

    /// <summary>The confirmation of "Use another save on this PC…" for <paramref name="source"/> (a listed save or a picked folder).</summary>
    private void AskUseOtherSave(GameDefinition game, SaveSource source, string title)
    {
        var c = SavesCulture;
        var level = LocalLevels.Of(game, source.Save, _install?.Root);
        var az = AzimuthSave();
        var to = S["sm.azimuth_name"];
        var fromLines = new List<CompareLine>
        {
            new(S["sm.kv.level"], level.Shown?.ToString("N0", c) ?? S["sm.unknown"]),
            new(S["sm.kv.credits"], source.Save.Credits?.ToString("N0", c) ?? S["sm.unknown"], Up: source.Save.Credits > az?.Credits, Down: source.Save.Credits < az?.Credits),
            new(S["sm.kv.modified"], When(source.Save.LastPlayedUtc)),
        };
        AskCopy(S.Format("sm.copy_title", ("from", title), ("to", to)), S.Format("sm.copy_sub", ("to", to)),
            new CompareSide(S.Format("sm.copy_from", ("name", title)), string.Empty, false, fromLines),
            new CompareSide(S.Format("sm.copy_replaced", ("name", to)), string.Empty, false, AzimuthSideLines()),
            S["copyask.backup_first"], S["othersave.use"], async () => await CopyOtherSaveAsync(game, source, title));
    }

    /// <summary>The confirmation of an old Noob cloud save's "Copy into my Azimuth save" (the cloud head's numbers as the row lists them).</summary>
    private void AskMoveOldSave(OldSaveRow row)
    {
        var c = SavesCulture;
        var to = S["sm.azimuth_name"];
        var from = row.Title;
        var save = row.Save;
        var fromLines = new List<CompareLine>
        {
            new(S["sm.kv.modified"], save.SavedAtUtc is { } at ? When(at) : S["sm.unknown"]),
            new(S["sm.kv.revision"], save.Line is { } line ? line.Revision.ToString(c) : S["sm.unknown"]),
        };
        AskCopy(S.Format("sm.copy_title", ("from", from), ("to", to)), S.Format("sm.copy_sub", ("to", to)),
            new CompareSide(S.Format("sm.copy_from", ("name", S.Format("export.copy_from_cloud", ("name", save.Gamertag)))), string.Empty, false, fromLines),
            new CompareSide(S.Format("sm.copy_replaced", ("name", to)), string.Empty, false, AzimuthSideLines()),
            S["copyask.backup_first"], S["sm.copy_confirm"], async () => await MoveOldSaveAsync(row));
    }

    /// <summary>
    /// A HARDCORE profile's own save onto Azimuth's (as before OD-239), named by its mode, profile and folder in every label. Only a HARDCORE
    /// row: an AZIMUTH profile's old save moves through <see cref="MoveOldSaveAsync"/> only.
    /// </summary>
    private void OpenCopyFromProfile(SaveSlot slot, ProfileRow row)
    {
        if (slot != SaveSlot.Hardcore || !row.Hardcore) return;
        if (LocalSaveOf(row.Profile.Xuid) is not { } local) { Message = S["sm.block.NoSource"]; return; }
        OpenCopy(new CopyPlan(slot, S.Format("sm.copy_source", ("mode", ModeWord(slot)), ("name", row.Gamertag)), local, null, null, null, false));
    }

    private void OpenCopy(CopyPlan plan)
    {
        RefreshGameRunning();
        _copy = plan;
        var c = SavesCulture;
        var to = S["sm.azimuth_name"];
        var az = AzimuthSave();
        var fromLines = new List<CompareLine>();
        if (plan.Cloud is { } rev)
        {
            fromLines.Add(new(S["sm.kv.level"], rev.Summary.ShownLevel(CurrentGame)?.ToString("N0", c) ?? S["sm.unknown"]));
            fromLines.Add(new(S["sm.kv.credits"], rev.Summary.Credits?.ToString("N0", c) ?? S["sm.unknown"], Up: rev.Summary.Credits > az?.Credits));
            fromLines.Add(new(S["sm.kv.modified"], When(rev.CommittedAt)));
            fromLines.Add(new(S["sm.kv.revision"], rev.Revision.ToString(c)));
        }
        else
        {
            var source = plan.Local ?? (plan.Entry is { } entry ? LocalHistory.NewestSave(entry) : null);
            fromLines.Add(new(S["sm.kv.credits"], source?.Credits?.ToString("N0", c) ?? S["sm.unknown"], Up: source?.Credits > az?.Credits, Down: source?.Credits < az?.Credits));
            fromLines.Add(new(S["sm.kv.modified"], source is null ? (plan.Entry is { } e ? When(e.Entry.At) : S["sm.unknown"]) : When(source.LastPlayedUtc)));
            fromLines.Add(new(S["sm.kv.revision"], source is null ? S["sm.unknown"] : S.Format("sm.local_rev", ("n", source.Revision.Number.ToString(c)))));
        }
        // owner 2026-11-12: the comparison is the centred confirmation popup over the screen it was opened from (no Copy page)
        AskCopy(S.Format("sm.copy_title", ("from", plan.FromName), ("to", to)), S.Format("sm.copy_sub", ("to", to)),
            new CompareSide(S.Format("sm.copy_from", ("name", plan.FromName)), string.Empty, false, fromLines),
            new CompareSide(S.Format("sm.copy_replaced", ("name", to)), string.Empty, false, AzimuthSideLines()),
            S.Format("sm.copy_note", ("to", to)), S["sm.copy_confirm"], () => CopyPlanAsync(plan));
    }

    /// <summary>
    /// The confirmation popup's Confirm: refused while the game runs (the popup stays, saying why); else it closes and its write runs (under the
    /// busy overlay, the Azimuth save backed up first).
    /// </summary>
    public async Task ConfirmCopyAsync()
    {
        if (_copyAsk is not { } ask) return;
        await RefreshGameRunningNowAsync();
        if (_gameRunning) { Message = S["saves.game_running"]; ChangedSaves(); return; }
        _copyAsk = null;
        ChangedSaves();
        Log("saves: copy confirmed");
        await ask.Run();
        _copy = null;
        ChangedSaves();
    }

    /// <summary>"Copy and replace": Azimuth's save archived first (verified), then the copy; nothing is written when the archive fails.</summary>
    private async Task CopyPlanAsync(CopyPlan plan)
    {
        if (SavesRootNow is not { } root) return;
        var fromWord = plan.From == SaveSlot.Azimuth ? "azimuth" : plan.From.ToString().ToLowerInvariant();
        await Run(S["busy.copying"], async () =>
        {
            try
            {
                if (plan.Original)
                {
                    var history = History();
                    await Task.Run(() => history.RestoreOriginal(root, DateTime.UtcNow));
                    Log($"saves: Azimuth's original save of {CurrentGame.Id} restored");
                    Done(S["sm.restored_original"]);
                    return;
                }
                // review 3.1.1: a profile's save is copied as it is now (a fresh scan under this overlay), never the comparison's cached one
                var local = plan.Local;
                if (local is not null && plan.Entry is null && plan.Cloud is null)
                {
                    local = (await FreshSavesListAsync(root)).FirstOrDefault(s => string.Equals(s.Folder, plan.Local!.Folder, StringComparison.OrdinalIgnoreCase));
                    if (local is null) { Log("saves: copy stopped: " + plan.Local!.Folder + " has no sealed save any more"); Message = S["sm.block.NoSource"]; return; }
                    if (local.Revision.Number != plan.Local!.Revision.Number)
                        Log($"saves: copy source {local.Folder} moved on since the comparison (r{plan.Local.Revision.Number} -> r{local.Revision.Number}); its newest is copied");
                }
                if (!await ArchiveFirstAsync(plan.Entry is not null || plan.Cloud is not null ? "restore" : "copy", AzimuthFolder, "sm.reason.copy",
                    [("from", plan.FromName), ("to", "Azimuth")], fromWord, "azimuth")) return;
                if (plan.Cloud is { } rev && plan.CloudXuid is { } xuid)
                {
                    if (_session is not { } session || CloudSaveClient() is not { } client) { Message = S["careers.download_failed"]; return; }
                    var access = await session.Tokens.AccessAsync(CancellationToken.None);
                    var outcome = await client.Saves.InstallRevisionOntoAsync(access, root, xuid, rev.Revision, rev.ContentSha256, AzimuthSaves.OfflineXuid, "before-history-copy");
                    Log($"saves: cloud r{rev.Revision} → Azimuth {(outcome.Failure is null ? "installed" : outcome.Failure.Code)}");
                    if (outcome.Failure is not null) { Message = S.Format("sm.copy_failed", ("reason", outcome.Failure.Message)); return; }
                }
                else
                {
                    var source = local ?? (plan.Entry is { } entry ? await Task.Run(() => LocalHistory.NewestSave(entry)) : null);
                    if (source is null) { Message = S["sm.block.NoSource"]; return; }
                    var result = await Task.Run(() => AzimuthSaves.Clone(root, source, AzimuthSaves.OfflineXuid, DateTime.UtcNow));
                    Log($"saves: copy {fromWord} → Azimuth r{result.Number}");
                }
                Done(S.Format("sm.copied", ("from", plan.FromName), ("to", S["sm.azimuth_name"])));
            }
            catch (Exception e) when (e is HistoryException or SaveException or IOException or UnauthorizedAccessException)
            {
                Log("saves: copy stopped: " + e.Message);
                Message = S.Format("sm.copy_failed", ("reason", e.Message));
            }
        });
    }

    public void OpenRestoreOriginal() => _ = OpenRestoreOriginalAsync();

    /// <summary>The original's entry and its newest save are read on a worker (every file verified), then the comparison opens.</summary>
    private async Task OpenRestoreOriginalAsync()
    {
        var history = SafeHistory();
        if (history is null || !history.HasOriginal) { Message = S.Format("sm.restore_original_none", ("game", GameLabel)); return; }
        var (original, save) = await Task.Run(() =>
        {
            try
            {
                var item = history.List().FirstOrDefault(i => i.IsOriginal);
                return (item, item is null ? null : LocalHistory.NewestSave(item));
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException or SaveException) { return (null, null); }
        });
        OpenCopy(new CopyPlan(SaveSlot.Azimuth, S["sm.original_name"], save, original, null, null, true));
    }

    // ------------------------------------------------------------------ History (§2.7, §7; cloud revisions of SAVES_API §4.2)

    public string HistoryTitle => S.Format("sm.history_title", ("slot", ModeWord(_historySlot)));
    public bool HistoryHardcoreOn => _historySlot == SaveSlot.Hardcore;
    public bool HistoryAzimuthOn => _historySlot == SaveSlot.Azimuth;
    public bool HistoryEmpty => HistoryRows.Count == 0;
    public HistoryRow? HistorySelected => _historySelected;
    public bool HasHistorySelected => _historySelected is not null;
    public string HistoryDetailTitle => _historySelected is { } r ? ModeWord(_historySlot) + " · " + r.Origin : string.Empty;
    public string HistoryDetailHeadline => _historySelected?.Title ?? string.Empty;
    public string HistoryDetailLine => _historySelected is { } r ? r.Detail + "  ·  " + r.When : string.Empty;
    public string HistoryDetailNote => _historySelected?.Cloud == true ? S["sm.history_cloud_note"] : S["sm.history_local_note"];
    public bool HistoryCanOpenFolder => _historySelected is { Cloud: false };
    public bool HistoryCanCopyToAzimuth => _historySelected is { } r && !_gameRunning && Idle
        && (r.Cloud ? r.Revision is not null && CloudSaveClient() is not null : r.Item is { } i && (i.IsOriginal || r.Save is not null));
    public string HistoryCopyBlocked => _historySelected is null ? string.Empty : _gameRunning ? S["sm.block.GameRunning"]
        : HistoryCanCopyToAzimuth ? string.Empty : S["sm.block.NoSource"];
    public bool HasHistoryCopyBlocked => !string.IsNullOrEmpty(HistoryCopyBlocked);
    public string HistoryCloudCopyNote => _historySlot == SaveSlot.Azimuth ? string.Empty : S["sm.history_cloud_copy"];
    public bool HasHistoryCloudCopyNote => !string.IsNullOrEmpty(HistoryCloudCopyNote) && _historySelected is not null;
    public Brush HistoryAccent => SlotBrushes.Accent(_historySlot);

    public async Task OpenHistoryAsync(SaveSlot slot)
    {
        RefreshGameRunning();
        if (slot == SaveSlot.Noob) slot = SaveSlot.Azimuth; // OD-239: the AZIMUTH profile has no cloud history; it plays Azimuth's own save
        _historySlot = slot;
        SavesGo(SavesScreen.History);
        // cached revisions render at once (no request, no overlay); a first read shows the overlay
        var row = slot == SaveSlot.Hardcore ? ProfileRows.FirstOrDefault(r => r.Card?.Player.Mode == "hardcore") : null;
        if (row is null || row.Card?.CareerRow is null || _remote.TryGet<IReadOnlyList<SaveRevision>>(RevisionsKey(CurrentGame.Id, row.Profile.Xuid), out _))
            await FillHistoryAsync();
        else await RunOrStep(S["busy.history"], FillHistoryAsync);
    }

    public Task ShowHistorySlotAsync(SaveSlot slot)
    {
        _historySlot = slot == SaveSlot.Noob ? SaveSlot.Azimuth : slot;
        return FillHistoryAsync();
    }

    public void SelectHistory(HistoryRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        _historySelected = row;
        var rows = HistoryRows.Select(r => r with { Selected = ReferenceEquals(r.Item, row.Item) && Equals(r.Revision, row.Revision) && r.Title == row.Title }).ToList();
        HistoryRows.Clear();
        foreach (var r in rows) HistoryRows.Add(r);
        ChangedHistory();
    }

    /// <summary>The slot's timeline: this PC's history entries of the slot, then (profile slots) the cloud's revisions, newest first.</summary>
    private async Task FillHistoryAsync()
    {
        var generation = ++_historyFill;
        HistoryRows.Clear();
        _historySelected = null;
        var rows = new List<(DateTimeOffset At, HistoryRow Row)>();
        // OD-239: only the HARDCORE slot has cloud revisions; the Azimuth slot is this PC's history only
        var row = _historySlot == SaveSlot.Hardcore ? ProfileRows.FirstOrDefault(r => r.Card?.Player.Mode == "hardcore") : null;
        if (SafeHistory() is { } history)
        {
            // owner 2026-11-10: the entries are read and each one's newest save verified (every file hashed) on a worker
            var azimuthSlot = _historySlot == SaveSlot.Azimuth;
            var xuid = row?.Profile.Xuid;
            var local = await Task.Run(() =>
            {
                try
                {
                    return history.List().Where(item => azimuthSlot ? item.Entry.Profile is null : item.Entry.Profile?.Xuid == xuid)
                        .Select(item => (Item: item, Save: LocalHistory.NewestSave(item))).ToList();
                }
                catch (Exception e) when (e is IOException or UnauthorizedAccessException or SaveException) { return []; }
            });
            if (generation != _historyFill) return; // a newer fill (another slot, a refresh) owns the list now
            foreach (var (item, save) in local)
            {
                var at = DateTimeOffset.TryParse(item.Entry.At, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var w) ? w : DateTimeOffset.MinValue;
                var title = item.IsOriginal ? S["sm.original_name"]
                    : save?.Credits is { } cr ? S.Format("sm.stats_credits", ("credits", cr.ToString("N0", SavesCulture)))
                    : item.Entry.Empty == true ? S["sm.history_empty_entry"] : Size(item.Bytes);
                rows.Add((at, new HistoryRow(S["sm.origin.pc"], title, item.Entry.Reason.For(S.Locale), false) { Item = item, Save = save, When = When(item.Entry.At) }));
            }
        }
        var read = _historyRead;
        _historyRead = CacheRead.Cached;
        if (_cloudHistoryXuid is { } seeded && row is not null && seeded == row.Profile.Xuid)
            rows.AddRange(_cloudHistory.Select(CloudRow)); // the harness's synthetic revisions
        else if (row?.Card?.CareerRow is not null && _session is { } session && CloudSaveClient() is { } client && ParseXuid(row.Profile) is not null)
        {
            // owner 2026-11-08: the revisions are read once and kept (by game and profile); our own save writes and the Refresh read them again
            var profile = row.Profile;
            try
            {
                var revisions = await _remote.GetAsync<IReadOnlyList<SaveRevision>>(RevisionsKey(CurrentGame.Id, profile.Xuid), async () =>
                {
                    var access = await session.Tokens.AccessAsync(CancellationToken.None);
                    var page = await client.Api.RevisionsAsync(access, profile.Xuid, null, 50);
                    return page.Ok && page.Value is { } value ? value.Items
                        : throw new RevisionsUnavailableException(page.Error?.Code ?? ((int)page.Status).ToString(CultureInfo.InvariantCulture));
                }, read, refreshed: fresh => { if (ShowSaveManager && _nav.Current == SavesScreen.History) _ = FillHistoryAsync(); });
                rows.AddRange(revisions.Select(CloudRow));
            }
            catch (RevisionsUnavailableException e) { Log($"saves: cloud history of {profile.ProfileId} → {e.Code}"); }
            catch (Exception e) when (e is HttpRequestException or ApiException or IOException or TaskCanceledException)
            {
                Log("saves: cloud history not read: " + e.GetType().Name);
            }
        }
        if (generation != _historyFill) return;
        HistoryRows.Clear();
        foreach (var (_, r) in rows.OrderByDescending(r => r.At)) HistoryRows.Add(r);
        if (HistoryRows.Count > 0) SelectHistory(HistoryRows[0]);
        ChangedHistory();
    }

    /// <summary>The newest history fill; an older one that resumes after it changes nothing.</summary>
    private int _historyFill;

    private string? _cloudHistoryXuid;

    /// <summary>How the next history fill reads the cloud revisions (the Refresh button asks the server, throttled).</summary>
    private CacheRead _historyRead = CacheRead.Cached;

    /// <summary>History's "Refresh": the cloud revisions again (at most every 10 s), under the busy overlay.</summary>
    public Task RefreshHistoryAsync()
    {
        _historyRead = CacheRead.Refresh;
        return RunOrStep(S["busy.history"], FillHistoryAsync);
    }

    /// <summary>Harness seam: the cloud revisions of <paramref name="xuid"/> as SAVES_API §4.2 lists them.</summary>
    internal void SeedCloudHistory(string xuid, IEnumerable<SaveRevision> revisions)
    {
        _cloudHistory.Clear();
        _cloudHistory.AddRange(revisions);
        _cloudHistoryXuid = xuid;
    }

    private (DateTimeOffset, HistoryRow) CloudRow(SaveRevision rev)
    {
        var at = DateTimeOffset.TryParse(rev.CommittedAt, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var w) ? w : DateTimeOffset.MinValue;
        var c = SavesCulture;
        var parts = new List<string>();
        if (rev.Summary.ShownLevel(CurrentGame) is { } level) parts.Add(S.Format("sm.stats_level", ("level", level.ToString("N0", c))));
        if (rev.Summary.Credits is { } cr) parts.Add(S.Format("sm.stats_credits", ("credits", cr.ToString("N0", c))));
        var kind = KindText(rev.Kind) + "  ·  " + S.Format("sm.revision_n", ("n", rev.Revision.ToString(c)))
            + (rev.DeviceLabel is { Length: > 0 } device ? "  ·  " + device : string.Empty);
        return (at, new HistoryRow(S["sm.origin.cloud"], parts.Count > 0 ? string.Join("  ·  ", parts) : S.Format("sm.revision_n", ("n", rev.Revision.ToString(c))), kind, true)
        { Revision = rev, When = When(rev.CommittedAt) });
    }

    private string KindText(string kind) => S["sm.kind." + kind] is var text && text != "sm.kind." + kind ? text : kind;

    public void HistoryOpenFolder()
    {
        if (_historySelected?.Item is not { } item) return;
        try { System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", "\"" + item.Folder + "\"") { UseShellExecute = true })?.Dispose(); }
        catch (Exception e) when (e is System.ComponentModel.Win32Exception or InvalidOperationException) { Log("saves: Explorer not opened: " + e.GetType().Name); }
    }

    public void HistoryCopyToAzimuth()
    {
        if (_historySelected is not { } r || !HistoryCanCopyToAzimuth) return;
        var name = ModeWord(_historySlot) + " · " + r.Origin + " · " + r.When;
        if (r.Cloud && r.Revision is { } rev)
        {
            var row = _historySlot == SaveSlot.Hardcore ? ProfileRows.FirstOrDefault(p => p.Card?.Player.Mode == "hardcore") : null;
            if (row is null || ParseXuid(row.Profile) is not { } xuid) return;
            OpenCopy(new CopyPlan(_historySlot, name, null, null, rev, xuid, false));
        }
        // the entry's newest save as the row found it off the UI thread (OpenCopy and the copy would otherwise verify it again here)
        else if (r.Item is { } item) OpenCopy(new CopyPlan(_historySlot, item.IsOriginal ? S["sm.original_name"] : name, r.Save, item, null, null, item.IsOriginal));
    }

    // ------------------------------------------------------------------ Delete (§4), Done

    public string DeleteRateNote => S["sm.delete_note_rate"];

    /// <summary>
    /// §7.1 "Deleting is archiving": a deleted career's save on this PC goes into its own game's history before the cleanup moves it to the
    /// saves folder's archive. The server already deleted the career, so a failed copy is logged and the cleanup (which never deletes) goes on.
    /// </summary>
    private void ArchiveDeletedCareer(Infinity.Core.Process.GameDefinition game, Profile row, bool allGames, string? installRoot)
    {
        if (installRoot is null || !ulong.TryParse(row.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)) return;
        if (xuid == AzimuthSaves.OfflineXuid) return; // OD-239: a profile's deletion never touches Azimuth's own save
        var root = AzimuthSaves.SavesRoot(installRoot, game);
        var folder = CloudSaveFiles.ProfileDir(root, xuid);
        if (!Directory.Exists(folder)) return;
        try
        {
            var history = new LocalHistory(_stateRoot, game, Version);
            history.EnsureOriginal(root, DateTime.UtcNow);
            var key = allGames ? "sm.reason.delete_profile" : "sm.reason.delete_career";
            var mode = row.Mode == "hardcore" ? "HARDCORE" : "AZIMUTH";
            var reason = new HistoryReason(StringsIn("en").Format(key, ("mode", mode), ("game", game.Name)), StringsIn("pt-BR").Format(key, ("mode", mode), ("game", game.Name)));
            var item = history.Archive(allGames ? "delete-profile" : "delete-career", folder, reason, DateTime.UtcNow,
                profile: new HistoryProfile(row.Xuid, ProfileName.Plain(row.Gamertag), row.Mode));
            Log($"profiles: history {game.Id} {item.Name}");
        }
        catch (Exception e) when (e is HistoryException or IOException or UnauthorizedAccessException)
        {
            Log($"profiles: history of the deleted {game.Id} career not written: " + e.Message);
        }
    }

    public string DoneText => _doneText;

    private void Done(string text)
    {
        _doneText = text;
        _copy = null;
        SavesGo(SavesScreen.Done);
        FillSaveCards();
    }

    // ------------------------------------------------------------------ Help Sensei (§4)

    public void HelpRestoreOriginal() => OpenRestoreOriginal();

    // ------------------------------------------------------------------ the HARDCORE check as a visible step list (§2.5)

    public IReadOnlyList<CheckStepRow> CheckSteps
    {
        get
        {
            var states = HardcoreCheck.Steps(_seedOffer?.Result.Outcome, _busy || _seedPhase != SeedPhase.None, _seedPhase == SeedPhase.Downloading);
            string[] keys = ["sm.check.read", "sm.check.progress", "sm.check.result"];
            return [.. states.Select((s, i) => new CheckStepRow(S[keys[i]], s))];
        }
    }

    public bool ShowCheckSteps => ShowSeedCheck || (ShowSeedChoice && (_busy || _seedPhase != SeedPhase.None));

    /// <summary>A refused check offers "Start a fresh HARDCORE career" and, when the source is a NOOB career, "Play it as NOOB".</summary>
    public bool ShowCheckWaysOut => HardcoreCheck.OffersWaysOut(_seedOffer?.Result.Outcome);

    public bool CanPlaySourceAsNoob => ShowCheckWaysOut && _seedOffer?.Source.Mode == "noob";

    public string CheckTrail => S["sm.crumb.home"] + "  ›  " + S["sm.crumb.start"] + "  ›  ";

    /// <summary>"Play it as NOOB": the check's source (the NOOB career) is chosen; the HARDCORE career stays as it was.</summary>
    public void PlaySourceAsNoob()
    {
        if (_seedOffer?.Source is not { Mode: "noob" } source) return;
        CloseSeedChoice();
        SavesHome();
        // OD-240: AZIMUTH is the account's one identity (its career of this game when it has one), whichever Noob career the check read
        var azimuth = AzimuthRow()?.Card?.CareerRow ?? source;
        Log($"saves: refused HARDCORE start, playing {azimuth.ProfileId} as AZIMUTH");
        ChooseProfile(azimuth);
        Message = S.Format("sm.check.as_noob_done", ("name", ProfileName.Plain(source.Gamertag)));
    }

    private void ChangedCheckSteps()
    {
        Changed(nameof(CheckSteps));
        Changed(nameof(ShowCheckSteps));
        Changed(nameof(ShowCheckWaysOut));
        Changed(nameof(CanPlaySourceAsNoob));
    }

    private void ChangedHistory()
    {
        Changed(nameof(HistoryTitle));
        Changed(nameof(HistoryHardcoreOn));
        Changed(nameof(HistoryAzimuthOn));

        Changed(nameof(HistoryEmpty));
        Changed(nameof(HistorySelected));
        Changed(nameof(HasHistorySelected));
        Changed(nameof(HistoryDetailTitle));
        Changed(nameof(HistoryDetailHeadline));
        Changed(nameof(HistoryDetailLine));
        Changed(nameof(HistoryDetailNote));
        Changed(nameof(HistoryCanOpenFolder));
        Changed(nameof(HistoryCanCopyToAzimuth));
        Changed(nameof(HistoryCopyBlocked));
        Changed(nameof(HasHistoryCopyBlocked));
        Changed(nameof(HistoryCloudCopyNote));
        Changed(nameof(HasHistoryCloudCopyNote));
        Changed(nameof(HistoryAccent));
    }

    private void ChangedSaves()
    {
        foreach (var name in new[]
        {
            nameof(SavesCurrent), nameof(ShowSaveManager), nameof(ShowPlainProfiles), nameof(ShowSavesHome), nameof(ShowSavesCreate), nameof(ShowSavesStart),
            nameof(ShowFloatingMenu),
            nameof(ShowSavesResolve), nameof(ShowCopyConfirm), nameof(AwaitingPlayer), nameof(CopyConfirmLabel), nameof(ShowOldNoobNotice), nameof(ShowSavesPick), nameof(PickTitle), nameof(PickSub), nameof(PickEmpty), nameof(CanUseOtherSave),
            nameof(ShowSavesHistory), nameof(ShowSavesDelete),
            nameof(ShowSavesHelp), nameof(ShowSavesDone), nameof(ShowHelpLink), nameof(SavesCanGoBack), nameof(SavesTrail), nameof(SavesTrailLast),
            nameof(SavesSubtitle), nameof(ShowGameRunningNote), nameof(GameRunningText),
            nameof(CreateTitle), nameof(CreateSub), nameof(CreateAccent), nameof(ResolveCloud), nameof(ResolvePc), nameof(ResolveTitle), nameof(ResolveHardcore),
            nameof(CopyFromSide), nameof(CopyToSide), nameof(CopyTitle), nameof(CopySub), nameof(CopyNote), nameof(CanConfirmCopy),
            nameof(CopyBlockedText), nameof(ShowCopyBlocked), nameof(DoneText), nameof(OldSaveResultText), nameof(HasOldSaveResult),
            nameof(ShowStartFromProfile), nameof(StartFromProfileLabel), nameof(StartFromProfileHint), nameof(StartFromProfileEnabled),
            nameof(ShowStartFromPc), nameof(StartFromPcHint), nameof(StartFreshHint), nameof(StartAccent),
        }) Changed(name);

        ChangedHistory();
    }
}
