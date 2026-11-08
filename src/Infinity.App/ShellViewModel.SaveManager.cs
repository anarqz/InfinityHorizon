// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Windows.Media;
using Infinity.Core.Api;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>The accent of a slot: HARDCORE orange, NOOB green, Azimuth blue (the prototype's palette), with their soft fills.</summary>
public static class SlotBrushes
{
    public static readonly SolidColorBrush Hardcore = Frozen(0xFF, 0xFF, 0x6A, 0x3D);
    public static readonly SolidColorBrush HardcoreSoft = Frozen(0x1F, 0xFF, 0x6A, 0x3D);
    public static readonly SolidColorBrush Noob = Frozen(0xFF, 0x3D, 0xDC, 0x97);
    public static readonly SolidColorBrush NoobSoft = Frozen(0x1F, 0x3D, 0xDC, 0x97);
    public static readonly SolidColorBrush Azimuth = Frozen(0xFF, 0x5A, 0xA8, 0xFF);
    public static readonly SolidColorBrush AzimuthSoft = Frozen(0x1F, 0x5A, 0xA8, 0xFF);

    public static SolidColorBrush Accent(SaveSlot slot) => slot switch { SaveSlot.Hardcore => Hardcore, SaveSlot.Noob => Noob, _ => Azimuth };

    public static SolidColorBrush Soft(SaveSlot slot) => slot switch { SaveSlot.Hardcore => HardcoreSoft, SaveSlot.Noob => NoobSoft, _ => AzimuthSoft };

    private static SolidColorBrush Frozen(byte a, byte r, byte g, byte b)
    {
        var brush = new SolidColorBrush(Color.FromArgb(a, r, g, b));
        brush.Freeze();
        return brush;
    }
}

/// <summary>One action of a card's ⋯ menu: its words, whether it can run now, and why not.</summary>
public sealed record MenuAction(string Id, string Label, bool Enabled, string? Reason = null, bool Danger = false)
{
    public bool HasReason => !string.IsNullOrEmpty(Reason);
}

/// <summary>A profile card of the save manager's home (SAVE_MANAGER_UX.md §4 "Saves (home)"), or the explainer of a missing mode.</summary>
public sealed record SaveCard(SaveSlot Slot, string ModeLabel)
{
    public ProfileRow? Row { get; init; }
    public Brush Accent => SlotBrushes.Accent(Slot);
    public Brush Soft => SlotBrushes.Soft(Slot);
    public bool IsExplainer => Row is null;
    public bool IsProfile => Row is not null;
    public string Gamertag => Row?.Gamertag ?? string.Empty;
    public string Initial => Gamertag.Length > 0 ? Gamertag[..1].ToUpperInvariant() : "?";
    public string Hint { get; init; } = string.Empty;
    public string Stats { get; init; } = string.Empty;
    public bool HasStats => !string.IsNullOrEmpty(Stats);
    public string ChipText { get; init; } = string.Empty;
    public ChipTone ChipTone { get; init; }
    public bool HasChip => !string.IsNullOrEmpty(ChipText);
    public bool ChipGood => ChipTone == ChipTone.Good;
    public bool ChipWarn => ChipTone == ChipTone.Warn;
    public bool ChipNeutral => ChipTone == ChipTone.Neutral;
    /// <summary>No career of the chosen game: the headline the card shows instead of the stats.</summary>
    public string NoCareerTitle { get; init; } = string.Empty;
    public string NoCareerBody { get; init; } = string.Empty;
    public bool HasNoCareer => !string.IsNullOrEmpty(NoCareerTitle);
    public string PrimaryLabel { get; init; } = string.Empty;
    public bool PrimaryEnabled { get; init; }
    /// <summary>play, resolve, start, none</summary>
    public string PrimaryKind { get; init; } = "play";
    public bool HasPrimary => PrimaryKind != "none";
    public bool Selected { get; init; }
    public bool MenuOpen { get; init; }
    public bool HasMenu => Row is not null && Menu.Count > 0;
    public IReadOnlyList<MenuAction> Menu { get; init; } = [];
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

/// <summary>One of the three slots of Manage saves.</summary>
public sealed record SlotCard(SaveSlot Slot, string Title, string Headline, string Detail)
{
    public Brush Accent => SlotBrushes.Accent(Slot);
    public Brush Soft => SlotBrushes.Soft(Slot);
    public IReadOnlyList<MenuAction> Actions { get; init; } = [];
    public string Reasons { get; init; } = string.Empty;
    public bool HasReasons => !string.IsNullOrEmpty(Reasons);
    public bool IsAzimuth => Slot == SaveSlot.Azimuth;
    public bool CanRestoreOriginal { get; init; }
    public string RestoreOriginalNote { get; init; } = string.Empty;
}

/// <summary>A line of the History timeline: a local entry or a cloud revision.</summary>
public sealed record HistoryRow(string Origin, string Title, string Detail, bool Cloud)
{
    public HistoryItem? Item { get; init; }
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
        CheckStepState.Done => SlotBrushes.Noob,
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
/// SAVE_MANAGER_UX.md (OD-207) as the Profiles page under OD-214: sign in → pick a game → pick a profile. The two profiles are cards with the
/// chosen game's career on each (avatar, gamertag, level · credits, a plain-language chip, one primary "Play as", a ⋯ menu); a missing
/// mode is an explainer; the Azimuth strip shows this PC's save of the game's title. Every non-root screen has ‹ Back and a breadcrumb
/// (Esc does the same). Copies write only Azimuth's save on this PC, each one archived first in the launcher's own history (§7); a copy into
/// a cloud career that exists, the server's per-revision reasons and Delete-from-history wait for the server (needs MACBOOK, see
/// docs/launcher/batch-1107/ITEM10_DESIGN_GAPS.md).
/// </summary>
public sealed partial class ShellViewModel
{
    private readonly SavesNav _nav = new();
    private string? _menuFor;
    private ProfileRow? _subject;
    private SaveSlot _historySlot = SaveSlot.Azimuth;
    private HistoryRow? _historySelected;
    private CopyPlan? _copy;
    private string _doneText = string.Empty;
    private bool _gameRunning;
    private string _createMode = "noob";
    private ResolvePick _resolvePick;
    private readonly List<SaveRevision> _cloudHistory = [];

    /// <summary>A copy into Azimuth (this PC): from a profile's save on this PC, a history entry, a cloud revision, or Azimuth's original.</summary>
    private sealed record CopyPlan(SaveSlot From, string FromName, SaveProfile? Local, HistoryItem? Entry, SaveRevision? Cloud, ulong? CloudXuid, bool Original);

    /// <summary>Harness seam: the game shown as running without a process.</summary>
    internal bool SavesForceRunning { get; set; }

    public ObservableCollection<SaveCard> SaveCards { get; } = [];
    public ObservableCollection<SlotCard> SlotCards { get; } = [];
    public ObservableCollection<HistoryRow> HistoryRows { get; } = [];

    private CultureInfo SavesCulture => CultureInfo.GetCultureInfo(S.Locale.StartsWith("pt", StringComparison.OrdinalIgnoreCase) ? "pt-BR" : "en-US");

    // ------------------------------------------------------------------ navigation (§3)

    /// <summary>The screen shown: a screen whose subject is gone falls back to the root.</summary>
    public SavesScreen SavesCurrent => _nav.Current switch
    {
        SavesScreen.Start when _startRow is null => SavesScreen.Home,
        SavesScreen.Delete when !ShowDeleteProfile => SavesScreen.Home,
        SavesScreen.Resolve when _subject is null => SavesScreen.Home,
        SavesScreen.Copy when _copy is null => SavesScreen.Home,
        var s => s,
    };

    private bool On(SavesScreen screen) => ShowPickProfile && !OfflineMode && SavesCurrent == screen;

    /// <summary>The save manager draws the Profiles page (signed in to a server that lists <c>players</c>); offline keeps the plain list.</summary>
    public bool ShowSaveManager => ShowPickProfile && !OfflineMode && _session?.Me?.Players is not null;
    public bool ShowPlainProfiles => ShowPickProfile && !ShowSaveManager;
    public bool ShowSavesHome => ShowSaveManager && SavesCurrent == SavesScreen.Home;
    public bool ShowSavesCreate => ShowSaveManager && SavesCurrent == SavesScreen.Create;
    public bool ShowSavesStart => ShowSaveManager && SavesCurrent == SavesScreen.Start;
    public bool ShowSavesResolve => ShowSaveManager && SavesCurrent == SavesScreen.Resolve;
    public bool ShowSavesManage => ShowSaveManager && SavesCurrent == SavesScreen.Manage;
    public bool ShowSavesCopy => ShowSaveManager && SavesCurrent == SavesScreen.Copy;
    public bool ShowSavesHistory => ShowSaveManager && SavesCurrent == SavesScreen.History;
    public bool ShowSavesDelete => ShowSaveManager && SavesCurrent == SavesScreen.Delete;
    public bool ShowSavesHelp => ShowSaveManager && SavesCurrent == SavesScreen.Help;
    public bool ShowSavesDone => ShowSaveManager && SavesCurrent == SavesScreen.Done;
    public bool ShowHelpLink => ShowSaveManager && SavesCurrent != SavesScreen.Help;
    public bool SavesCanGoBack => SavesCurrent != SavesScreen.Home;

    private string ScreenTitle(SavesScreen screen) => S["sm.crumb." + screen.ToString().ToLowerInvariant()];

    /// <summary>The breadcrumb before the current screen ("Saves › Manage saves › "), empty at the root.</summary>
    public string SavesTrail
    {
        get
        {
            var trail = _nav.Trail;
            if (SavesCurrent == SavesScreen.Home) return string.Empty;
            return string.Concat(trail.Take(trail.Count - 1).Select(s => ScreenTitle(s) + "  ›  "));
        }
    }

    public string SavesTrailLast => ScreenTitle(SavesCurrent);

    public void SavesBack()
    {
        PruneSaves();
        _menuFor = null;
        switch (SavesCurrent)
        {
            case SavesScreen.Start: CancelCareerStart(); return; // pops itself
            case SavesScreen.Delete: CancelDeleteProfile(); return; // pops itself
            case SavesScreen.Copy: _copy = null; break;
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
        _menuFor = null;
        PruneSaves();
        _nav.Push(screen);
        Message = null;
        ChangedSaves();
    }

    public void SavesHome()
    {
        _nav.Root();
        _startRow = null; _copy = null; _subject = null; _menuFor = null;
        FillSaveCards();
        ChangedSaves();
    }

    public void OpenSavesHelp() => SavesGo(SavesScreen.Help);

    public void OpenManageSaves()
    {
        RefreshGameRunning();
        FillSlotCards();
        SavesGo(SavesScreen.Manage);
    }

    // ------------------------------------------------------------------ home (§4)

    public string SavesSubtitle => S.Format("sm.subtitle", ("game", GameLabel));

    public bool SavesEmpty => _session?.Me?.Players is { } players && !players.Any(p => p.Status == "active");

    public string AzimuthStripLine { get; private set; } = string.Empty;

    public bool ShowGameRunningNote => _gameRunning && ShowSaveManager;

    public string GameRunningText => S.Format("sm.running", ("game", GameLabel));

    private void RefreshGameRunning()
    {
        bool running;
        try { running = SavesForceRunning || Playing || System.Diagnostics.Process.GetProcessesByName("azimuth").Length > 0; }
        catch (InvalidOperationException) { running = Playing; }
        _gameRunning = running;
    }

    /// <summary>The cards of the home, rebuilt from <see cref="ProfileRows"/> (HARDCORE first, then NOOB; a missing mode is its explainer).</summary>
    private void FillSaveCards()
    {
        SaveCards.Clear();
        if (OfflineMode || _session?.Me?.Players is null) { ChangedSaves(); return; }
        RefreshGameRunning();
        EnsureOriginalQuietly();
        var game = CurrentGame;
        foreach (var (slot, mode) in new[] { (SaveSlot.Hardcore, "hardcore"), (SaveSlot.Noob, "noob") })
        {
            var rows = ProfileRows.Where(r => r.Card?.Player.Mode == mode).ToList();
            if (rows.Count == 0) { SaveCards.Add(Explainer(slot)); continue; }
            foreach (var row in rows) SaveCards.Add(CardOf(slot, row));
        }
        AzimuthStripLine = StripLine();
        ChangedSaves();
    }

    private string ModeWord(SaveSlot slot) => S[slot == SaveSlot.Hardcore ? "sm.hardcore" : slot == SaveSlot.Noob ? "sm.noob" : "sm.azimuth"];

    private SaveCard Explainer(SaveSlot slot)
    {
        var hardcore = slot == SaveSlot.Hardcore;
        var offered = !hardcore || (CurrentGame.HardcoreCareers && CurrentGame.FullFeatures);
        return new SaveCard(slot, ModeWord(slot))
        {
            ExplainTitle = S[hardcore ? "sm.explain.hc_title" : "sm.explain.noob_title"],
            ExplainBody = S[hardcore ? "sm.explain.hc_body" : "sm.explain.noob_body"],
            CreateLabel = S.Format("sm.explain.create", ("mode", ModeWord(slot))),
            CreateEnabled = offered,
            CreateNote = offered ? string.Empty : S.Format("sm.no_hardcore_hint", ("game", GameLabel)),
        };
    }

    private SaveCard CardOf(SaveSlot slot, ProfileRow row)
    {
        var card = row.Card!;
        var chip = ChipOf(row);
        var selected = _gate.ProfileKey is { } key && key == card.CareerId;
        string primary; string kind; bool enabled;
        switch (card.State)
        {
            case CareerState.Started when SaveChips.NeedsResolve(chip):
                (primary, kind, enabled) = (S["sm.resolve"], "resolve", true);
                break;
            case CareerState.Started:
                (primary, kind, enabled) = (selected ? S["sm.playing_as"] : S.Format("sm.play_as", ("mode", ModeWord(slot))), "play", !selected);
                break;
            case CareerState.NotOffered:
                (primary, kind, enabled) = (S.Format("sm.no_hardcore", ("game", GameLabel)), "none", false);
                break;
            default:
                (primary, kind, enabled) = (S.Format("sm.start_career", ("game", GameLabel)), "start", card.Startable);
                break;
        }
        return new SaveCard(slot, ModeWord(slot))
        {
            Row = row,
            Hint = S[slot == SaveSlot.Hardcore ? "sm.hint_hardcore" : "sm.hint_noob"],
            Stats = card.State == CareerState.Started ? StatsOf(card.Career) : string.Empty,
            ChipText = ChipText(chip, card.Career),
            ChipTone = SaveChips.Tone(chip),
            NoCareerTitle = card.State switch
            {
                CareerState.None => S.Format("sm.no_career", ("game", GameLabel)),
                CareerState.Empty => S["sm.not_started"],
                CareerState.NotOffered => S.Format("sm.no_hardcore", ("game", GameLabel)),
                _ => string.Empty,
            },
            NoCareerBody = card.State switch
            {
                CareerState.None or CareerState.Empty => S[card.Offers.FromExisting ? "sm.no_career_hint" : "sm.no_career_hint_fresh"],
                CareerState.NotOffered => S.Format("sm.no_hardcore_hint", ("game", GameLabel)),
                _ => string.Empty,
            },
            PrimaryLabel = primary,
            PrimaryKind = kind,
            PrimaryEnabled = enabled,
            Selected = selected,
            MenuOpen = _menuFor == card.Player.ProfileId,
            Menu = MenuOf(slot, row, chip),
            Note = card.Duplicate ? S["careers.duplicate"] : string.Empty,
        };
    }

    private List<MenuAction> MenuOf(SaveSlot slot, ProfileRow row, SaveChipKind chip)
    {
        var card = row.Card!;
        var list = new List<MenuAction>();
        var running = _gameRunning;
        if (card.State == CareerState.Started)
        {
            var local = LocalSaveOf(card.Player.Xuid) is not null;
            list.Add(new MenuAction("copy-azimuth", S["sm.menu.copy_azimuth"], local && !running,
                running ? S["sm.block.GameRunning"] : local ? null : S["sm.block.NoSource"]));
            var other = slot == SaveSlot.Hardcore ? SaveSlot.Noob : SaveSlot.Hardcore;
            var target = CopyTargets.Of(slot, Facts()).FirstOrDefault(t => t.To == other);
            if (target is { Block: CopyBlock.StartInstead })
                list.Add(new MenuAction("start-other", S.Format("sm.menu.start_other", ("mode", ModeWord(other))), true));
            if (SaveChips.CanDownload(chip) && CurrentGame.FullFeatures)
                list.Add(new MenuAction("download", S["sm.menu.download"], !running, running ? S["sm.block.GameRunning"] : null));
        }
        list.Add(new MenuAction("history", S["sm.menu.history"], true));
        if (row.CanDeleteCareer) list.Add(new MenuAction("delete-career", S.Format("sm.menu.delete_career", ("game", GameLabel)), CanAskDeleteProfile, Danger: true));
        if (row.CanDeleteWholeProfile) list.Add(new MenuAction("delete-profile", S["sm.menu.delete_profile"], CanAskDeleteProfile, Danger: true));
        return list;
    }

    private string StatsOf(Career? career)
    {
        if (career?.Progress is not { } p) return S["sm.stats_none"];
        var c = SavesCulture;
        var parts = new List<string>();
        if (p.Level is { } level) parts.Add(S.Format("sm.stats_level", ("level", level.ToString("N0", c))));
        if (p.Credits is { } credits) parts.Add(S.Format("sm.stats_credits", ("credits", credits.ToString("N0", c))));
        if (p.Cars is { } cars) parts.Add(S.Format("sm.stats_cars", ("cars", cars.ToString(c))));
        if (p.PlaySeconds is { } seconds) parts.Add(S.Format("sm.stats_hours", ("hours", (seconds / 3600).ToString(c))));
        return parts.Count == 0 ? S["sm.stats_none"] : string.Join("  ·  ", parts);
    }

    /// <summary>The chip of a card, from this PC's files and sync state only (no request).</summary>
    private SaveChipKind ChipOf(ProfileRow row)
    {
        var card = row.Card!;
        if (card.State != CareerState.Started) return SaveChipKind.None;
        string? content = null; SyncMark? mark = null; var pending = false;
        if (ulong.TryParse(card.Player.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid) && SavesRootNow is { } root)
        {
            try
            {
                if (AzimuthSaves.NewestSealed(CloudSaveFiles.ProfileDir(root, xuid)) is { } newest)
                    content = CurrentGame.FullFeatures ? Fh2Bundle.ContentDigest(CloudSaveFiles.ReadFiles(newest)) : "local";
            }
            catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException) { content = "unreadable"; }
            if (CurrentGame.FullFeatures && CloudSaveClient() is { } client && client.Saves.State(xuid) is { } state)
            {
                mark = state.Mark;
                pending = state.PendingUpload is not null && state.PendingUpload.Kind != CommitKinds.SeedFromProfile;
            }
        }
        return SaveChips.Of(new SaveChipInput(card.State, CurrentGame.FullFeatures, card.Career?.Cloud?.Revision, content, mark, pending, card.SeedPending));
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

    /// <summary>The newest save of <paramref name="xuid"/>'s folder on this PC (any game layout Azimuth writes), or null.</summary>
    private SaveProfile? LocalSaveOf(string xuid)
    {
        if (SavesRootNow is not { } root || !ulong.TryParse(xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var x)) return null;
        try { return AzimuthSaves.List(root).FirstOrDefault(s => s.Xuid == x); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    /// <summary>Azimuth's own player on this PC (<c>User_9000000000000</c>): what the game loads offline.</summary>
    private SaveProfile? AzimuthSave()
    {
        if (SavesRootNow is not { } root) return null;
        try { return AzimuthSaves.List(root).FirstOrDefault(s => s.IsOffline); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    private string StripLine()
    {
        if (_install is null) return S["sm.strip.no_install"];
        if (AzimuthSave() is not { } save) return S.Format("sm.strip.none", ("game", GameLabel));
        return save.Credits is { } credits
            ? S.Format("sm.strip.line", ("credits", credits.ToString("N0", SavesCulture)), ("when", When(save.LastPlayedUtc)))
            : S.Format("sm.strip.line_nocr", ("when", When(save.LastPlayedUtc)));
    }

    /// <summary>§7.2: the title's saves as first found, once (a failure is logged; the next action tries again and stops if it fails).</summary>
    private void EnsureOriginalQuietly()
    {
        if (SavesRootNow is not { } root) return;
        try
        {
            if (History().EnsureOriginal(root, DateTime.UtcNow) is { } original)
                Log($"saves: history original of {CurrentGame.Id}/{CurrentGame.TitleKey} kept ({original.Entry.Files.Count} files)");
        }
        catch (Exception e) when (e is HistoryException or IOException or UnauthorizedAccessException) { Log("saves: history original not kept: " + e.Message); }
    }

    /// <summary>
    /// §7.1 "Every write first archives": <paramref name="folder"/> into a new history entry, verified. False (and the reason shown) when it
    /// could not be kept: the caller then writes nothing.
    /// </summary>
    private bool ArchiveFirst(string action, string? folder, string reasonKey, (string, string)[] args, string? from = null, string? to = null, ProfileRow? row = null)
    {
        if (SavesRootNow is not { } root) return false;
        try
        {
            var history = History();
            history.EnsureOriginal(root, DateTime.UtcNow);
            var reason = new HistoryReason(StringsIn("en").Format(reasonKey, args), StringsIn("pt-BR").Format(reasonKey, args));
            var profile = row is null ? null : new HistoryProfile(row.Profile.Xuid, row.Gamertag, row.Profile.Mode);
            var item = history.Archive(action, folder, reason, DateTime.UtcNow, from, to, profile, slug: from is null ? null : from + " to " + to);
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

    public async Task SavesPrimaryAsync(SaveCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (card.Row is not { } row || !card.PrimaryEnabled) return;
        _menuFor = null;
        switch (card.PrimaryKind)
        {
            case "resolve":
                OpenResolve(row);
                return;
            case "start":
                PickProfileRow(row);
                if (_startRow is not null) SavesGo(SavesScreen.Start);
                return;
            default:
                PickProfileRow(row);
                await Task.CompletedTask;
                return;
        }
    }

    public void ToggleSavesMenu(SaveCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        var id = card.Row?.Card?.Player.ProfileId;
        _menuFor = _menuFor == id ? null : id;
        FillSaveCards();
    }

    public async Task SavesMenuAsync(SaveCard card, MenuAction action)
    {
        ArgumentNullException.ThrowIfNull(card);
        ArgumentNullException.ThrowIfNull(action);
        if (card.Row is not { } row || !action.Enabled) return;
        _menuFor = null;
        switch (action.Id)
        {
            case "copy-azimuth":
                OpenCopyFromProfile(card.Slot, row);
                break;
            case "start-other":
                StartOtherFrom(card.Slot);
                break;
            case "history":
                await OpenHistoryAsync(card.Slot);
                break;
            case "download":
                await DownloadCareerAsync(row);
                FillSaveCards();
                break;
            case "delete-career":
                AskDeleteProfile(row.Profile);
                break;
            case "delete-profile":
                AskDeleteWholeProfile(row.Profile);
                break;
        }
        ChangedSaves();
    }

    /// <summary>The explainer's "Create a HARDCORE/NOOB profile": the new-profile form with that mode.</summary>
    public void OpenCreateProfile(SaveCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (!card.CreateEnabled) return;
        _createMode = card.Slot == SaveSlot.Hardcore ? "hardcore" : "noob";
        NewHardcore = _createMode == "hardcore";
        SavesGo(SavesScreen.Create);
    }

    public string CreateTitle => S.Format("sm.create_title", ("mode", ModeWord(_createMode == "hardcore" ? SaveSlot.Hardcore : SaveSlot.Noob)));

    public Brush CreateAccent => SlotBrushes.Accent(_createMode == "hardcore" ? SaveSlot.Hardcore : SaveSlot.Noob);

    /// <summary>"Start the other profile from this" (the ⋯ menu): the other mode's Start screen with this profile as its source.</summary>
    private void StartOtherFrom(SaveSlot slot)
    {
        var other = slot == SaveSlot.Hardcore ? "noob" : "hardcore";
        if (ProfileRows.FirstOrDefault(r => r.Card?.Player.Mode == other && r.Card.Startable) is not { } target) return;
        PickProfileRow(target);
        if (_startRow is not null) SavesGo(SavesScreen.Start);
    }

    private SlotFacts Facts()
    {
        var hc = ProfileRows.FirstOrDefault(r => r.Card?.Player.Mode == "hardcore");
        var noob = ProfileRows.FirstOrDefault(r => r.Card?.Player.Mode == "noob");
        return new SlotFacts(
            hc is not null, hc?.Card?.CareerRow is not null, hc is not null && LocalSaveOf(hc.Profile.Xuid) is not null,
            noob is not null, noob?.Card?.CareerRow is not null, noob is not null && LocalSaveOf(noob.Profile.Xuid) is not null,
            AzimuthSave() is not null, _gameRunning, CurrentGame.FullFeatures, CurrentGame.HardcoreCareers,
            ProfileSeed.Offered(ServerAllowsSeedFromProfile, HardcoreKeepsCloud));
    }

    // ------------------------------------------------------------------ Start (§2.6, OD-214)

    public string StartSub => S["sm.start_sub"];

    public string StartFreshHint => S.Format("sm.start.fresh_hint", ("game", GameLabel));

    /// <summary>"From my NOOB/HARDCORE profile": HARDCORE only (CR-S52); NOOB starts from this PC's saves (the OD-41 picker).</summary>
    public bool ShowStartFromProfile => CareerStartHardcore;

    public string StartFromProfileLabel => S.Format("sm.start.from_profile", ("mode", ModeWord(SaveSlot.Noob)));

    public string StartFromProfileHint => CareerStartHardcore && CareerStartFromExisting ? CareerStartExistingHint
        : !CurrentGame.FullFeatures ? S.Format("sm.start.cloudless", ("game", GameLabel))
        : S.Format("sm.start.from_profile_off", ("mode", ModeWord(SaveSlot.Noob)), ("game", GameLabel));

    public bool StartFromProfileEnabled => CareerStartHardcore && CareerStartFromExisting && Idle;

    public bool StartFromPcEnabled => !CareerStartHardcore && CareerStartFromExisting && Idle;

    public string StartFromPcHint => CareerStartHardcore ? S["sm.start.from_pc_hc"]
        : CareerStartFromExisting ? S.Format("sm.start.from_pc_hint", ("game", GameLabel))
        : !CurrentGame.FullFeatures ? S.Format("sm.start.cloudless", ("game", GameLabel))
        : S.Format("sm.start.from_pc_none", ("game", GameLabel));

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
            new(S["sm.kv.level"], N(p?.Level)), new(S["sm.kv.credits"], N(p?.Credits)), new(S["sm.kv.cars"], N(p?.Cars)),
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
        RefreshGameRunning();
        if (_gameRunning) { Message = S["saves.game_running"]; return; }
        await Run(async () =>
        {
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            var (head, failure) = await client.Saves.HeadAsync(access, xuid);
            if (head is null || failure is not null || head.Revision == 0) { Message = S["careers.download_failed"]; return; }
            var root = AzimuthSaves.SavesRoot(_install.Root, CurrentGame);
            var decision = client.Saves.Decide(root, xuid, head);
            if (decision.Failure is not null) { Message = S["careers.download_failed"]; return; }
            if (!ArchiveFirst("resolve", ProfileFolder(row), "sm.reason.resolve_cloud", [("mode", ModeWordIn(row))], "this-pc", "cloud", row)) return;
            var outcome = await client.Saves.InstallHeadAsync(access, root, xuid, head, decision.LocalContent, "before-resolve");
            Log($"saves: resolve {row.Profile.Game} {row.Profile.ProfileId} keep cloud r{head.Revision} → {(outcome.Failure is null ? "installed" : outcome.Failure.Code)}");
            if (outcome.Failure is not null) { Message = S["careers.download_failed"]; return; }
            Done(S["sm.kept_cloud"]);
        });
    }

    /// <summary>"Keep this PC": nothing is written now; this PC's save goes up at the next Play (the session's upload, with the HARDCORE check).</summary>
    public void ResolveKeepPc()
    {
        if (_subject is not { } row) return;
        Log($"saves: resolve {row.Profile.Game} {row.Profile.ProfileId} keep this PC (uploaded at the next Play)");
        PickProfileRow(row);
        Message = S["sm.kept_pc"];
    }

    private static string ModeWordIn(ProfileRow row) => row.Hardcore ? "HARDCORE" : "NOOB";

    // ------------------------------------------------------------------ Manage saves (§4)

    public string ManageSub => S.Format("sm.manage_sub", ("game", GameLabel));

    public string HistorySizeText
    {
        get
        {
            try
            {
                var history = History();
                var items = history.List();
                return S.Format("sm.history_size", ("count", items.Count.ToString(SavesCulture)), ("size", Size(items.Sum(i => i.Bytes))));
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return string.Empty; }
        }
    }

    private string Size(long bytes) => bytes >= 1 << 20 ? (bytes / 1048576.0).ToString("0.0", SavesCulture) + " MB"
        : bytes >= 1024 ? (bytes / 1024.0).ToString("0", SavesCulture) + " KB" : bytes.ToString(SavesCulture) + " B";

    private void FillSlotCards()
    {
        SlotCards.Clear();
        var facts = Facts();
        var needsServer = false;
        foreach (var slot in new[] { SaveSlot.Hardcore, SaveSlot.Azimuth, SaveSlot.Noob })
        {
            var row = slot == SaveSlot.Azimuth ? null : ProfileRows.FirstOrDefault(r => r.Card?.Player.Mode == (slot == SaveSlot.Hardcore ? "hardcore" : "noob"));
            var title = slot == SaveSlot.Azimuth ? S["sm.slot.pc"] : S.Format("sm.slot.cloud", ("mode", ModeWord(slot)));
            string headline, detail;
            if (slot == SaveSlot.Azimuth)
            {
                var az = AzimuthSave();
                headline = az is null ? S["sm.slot.no_save"] : az.Credits is { } cr ? S.Format("sm.stats_credits", ("credits", cr.ToString("N0", SavesCulture))) : S["sm.azimuth_name"];
                detail = az is null ? S.Format("sm.strip.none", ("game", GameLabel)) : S.Format("sm.modified", ("when", When(az.LastPlayedUtc)));
            }
            else if (row is null) { headline = S["sm.slot.no_profile"]; detail = string.Empty; }
            else if (row.Card?.CareerRow is null) { headline = row.Gamertag; detail = row.Card?.State == CareerState.NotOffered ? S.Format("sm.no_hardcore", ("game", GameLabel)) : S.Format("sm.no_career", ("game", GameLabel)); }
            else
            {
                headline = row.Gamertag;
                detail = StatsOf(row.Card.Career) + (LocalSaveOf(row.Profile.Xuid) is { } local ? "\n" + S.Format("sm.modified", ("when", When(local.LastPlayedUtc))) : "\n" + S["sm.slot.no_save"]);
            }
            var targets = CopyTargets.Of(slot, facts);
            var actions = targets.Select(t => new MenuAction("copy:" + t.To,
                t.Block == CopyBlock.StartInstead ? S.Format("sm.start_instead", ("mode", ModeWord(t.To))) : S.Format("sm.copy_to", ("slot", t.To == SaveSlot.Azimuth ? S["sm.azimuth_short"] : ModeWord(t.To))),
                t.Enabled, t.Enabled ? null : BlockText(t.Block))).ToList();
            actions.Add(new MenuAction("history", S["sm.menu.history"], true));
            // one page note says why copies into existing cloud careers are off (needs MACBOOK); the card lists only its other reasons
            var reasons = string.Join("\n", targets.Where(t => !t.Enabled && t.Block != CopyBlock.NeedsServer)
                .Select(t => "• " + S.Format("sm.block.line", ("target", t.To == SaveSlot.Azimuth ? S["sm.azimuth_short"] : ModeWord(t.To)), ("reason", BlockText(t.Block)))));
            needsServer |= targets.Any(t => t.Block == CopyBlock.NeedsServer);
            var history = SafeHistory();
            SlotCards.Add(new SlotCard(slot, title, headline, detail)
            {
                Actions = actions,
                Reasons = reasons,
                CanRestoreOriginal = slot == SaveSlot.Azimuth && history?.HasOriginal == true && !_gameRunning,
                RestoreOriginalNote = slot != SaveSlot.Azimuth ? string.Empty : _gameRunning ? S["sm.block.GameRunning"]
                    : history?.HasOriginal == true ? S["sm.restore_original_hint"] : S.Format("sm.restore_original_none", ("game", GameLabel)),
            });
        }
        ManageServerNote = needsServer ? S["sm.manage_server_note"] : string.Empty;
        Changed(nameof(HistorySizeText));
        Changed(nameof(ManageServerNote));
        Changed(nameof(HasManageServerNote));
    }

    public string ManageServerNote { get; private set; } = string.Empty;

    public bool HasManageServerNote => !string.IsNullOrEmpty(ManageServerNote);

    private LocalHistory? SafeHistory()
    {
        try { return History(); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    private string BlockText(CopyBlock block) => S.Format("sm.block." + block, ("game", GameLabel));

    public async Task SlotActionAsync(SlotCard slot, MenuAction action)
    {
        ArgumentNullException.ThrowIfNull(slot);
        ArgumentNullException.ThrowIfNull(action);
        if (!action.Enabled) { if (action.Reason is { } why) Message = why; return; }
        if (action.Id == "history") { await OpenHistoryAsync(slot.Slot); return; }
        if (!Enum.TryParse<SaveSlot>(action.Id["copy:".Length..], out var to)) return;
        var block = CopyTargets.Of(slot.Slot, Facts()).First(t => t.To == to).Block;
        if (block == CopyBlock.StartInstead)
        {
            if (slot.Slot == SaveSlot.Azimuth || to == SaveSlot.Noob || to == SaveSlot.Hardcore)
            {
                var target = ProfileRows.FirstOrDefault(r => r.Card?.Player.Mode == (to == SaveSlot.Hardcore ? "hardcore" : "noob") && r.Card.Startable);
                if (target is null) return;
                PickProfileRow(target);
                if (_startRow is not null) SavesGo(SavesScreen.Start);
            }
            return;
        }
        if (to != SaveSlot.Azimuth) return;
        if (slot.Slot == SaveSlot.Azimuth) return;
        var row = ProfileRows.FirstOrDefault(r => r.Card?.Player.Mode == (slot.Slot == SaveSlot.Hardcore ? "hardcore" : "noob"));
        if (row is not null) OpenCopyFromProfile(slot.Slot, row);
    }

    // ------------------------------------------------------------------ Copy (§2.1: Compare → Check → Confirm; only into Azimuth today)

    public CompareSide? CopyFromSide { get; private set; }
    public CompareSide? CopyToSide { get; private set; }
    public string CopyTitle { get; private set; } = string.Empty;
    public string CopySub { get; private set; } = string.Empty;
    public string CopyNote { get; private set; } = string.Empty;
    public bool CanConfirmCopy => _copy is not null && !_gameRunning && Idle;
    public string CopyBlockedText => _gameRunning ? GameRunningText : string.Empty;
    public bool ShowCopyBlocked => _gameRunning && _copy is not null;

    private void OpenCopyFromProfile(SaveSlot slot, ProfileRow row)
    {
        if (LocalSaveOf(row.Profile.Xuid) is not { } local) { Message = S["sm.block.NoSource"]; return; }
        OpenCopy(new CopyPlan(slot, ModeWord(slot) + " · " + row.Gamertag, local, null, null, null, false));
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
            fromLines.Add(new(S["sm.kv.level"], rev.Summary.ShownLevel()?.ToString("N0", c) ?? S["sm.unknown"]));
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
        var toLines = new List<CompareLine>
        {
            new(S["sm.kv.credits"], az?.Credits?.ToString("N0", c) ?? S["sm.unknown"]),
            new(S["sm.kv.modified"], az is null ? S["sm.slot.no_save"] : When(az.LastPlayedUtc)),
            new(S["sm.kv.revision"], az is null ? S["sm.unknown"] : S.Format("sm.local_rev", ("n", az.Revision.Number.ToString(c)))),
        };
        CopyFromSide = new CompareSide(S.Format("sm.copy_from", ("name", plan.FromName)), string.Empty, false, fromLines);
        CopyToSide = new CompareSide(S.Format("sm.copy_replaced", ("name", to)), string.Empty, false, toLines);
        CopyTitle = S.Format("sm.copy_title", ("from", plan.FromName), ("to", to));
        CopySub = S.Format("sm.copy_sub", ("to", to));
        CopyNote = S.Format("sm.copy_note", ("to", to));
        SavesGo(SavesScreen.Copy);
    }

    /// <summary>"Copy and replace": Azimuth's save archived first (verified), then the copy; nothing is written when the archive fails.</summary>
    public async Task ConfirmCopyAsync()
    {
        if (_copy is not { } plan || SavesRootNow is not { } root) return;
        RefreshGameRunning();
        if (_gameRunning) { Message = S["saves.game_running"]; ChangedSaves(); return; }
        var fromWord = plan.From == SaveSlot.Azimuth ? "azimuth" : plan.From.ToString().ToLowerInvariant();
        await Run(async () =>
        {
            try
            {
                if (plan.Original)
                {
                    History().RestoreOriginal(root, DateTime.UtcNow);
                    Log($"saves: Azimuth's original save of {CurrentGame.Id} restored");
                    Done(S["sm.restored_original"]);
                    return;
                }
                if (!ArchiveFirst(plan.Entry is not null || plan.Cloud is not null ? "restore" : "copy", AzimuthFolder, "sm.reason.copy",
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
                    var source = plan.Local ?? (plan.Entry is { } entry ? LocalHistory.NewestSave(entry) : null);
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

    public void OpenRestoreOriginal()
    {
        var history = SafeHistory();
        if (history is null || !history.HasOriginal) { Message = S.Format("sm.restore_original_none", ("game", GameLabel)); return; }
        var original = history.List().FirstOrDefault(i => i.IsOriginal);
        OpenCopy(new CopyPlan(SaveSlot.Azimuth, S["sm.original_name"], null, original, null, null, true));
    }

    // ------------------------------------------------------------------ History (§2.7, §7; cloud revisions of SAVES_API §4.2)

    public string HistoryTitle => S.Format("sm.history_title", ("slot", ModeWord(_historySlot)));
    public bool HistoryHardcoreOn => _historySlot == SaveSlot.Hardcore;
    public bool HistoryAzimuthOn => _historySlot == SaveSlot.Azimuth;
    public bool HistoryNoobOn => _historySlot == SaveSlot.Noob;
    public bool HistoryEmpty => HistoryRows.Count == 0;
    public HistoryRow? HistorySelected => _historySelected;
    public bool HasHistorySelected => _historySelected is not null;
    public string HistoryDetailTitle => _historySelected is { } r ? ModeWord(_historySlot) + " · " + r.Origin : string.Empty;
    public string HistoryDetailHeadline => _historySelected?.Title ?? string.Empty;
    public string HistoryDetailLine => _historySelected is { } r ? r.Detail + "  ·  " + r.When : string.Empty;
    public string HistoryDetailNote => _historySelected?.Cloud == true ? S["sm.history_cloud_note"] : S["sm.history_local_note"];
    public bool HistoryCanOpenFolder => _historySelected is { Cloud: false };
    public bool HistoryCanCopyToAzimuth => _historySelected is { } r && !_gameRunning && Idle
        && (r.Cloud ? r.Revision is not null && CloudSaveClient() is not null : r.Item is { } i && (i.IsOriginal || LocalHistory.NewestSave(i) is not null));
    public string HistoryCopyBlocked => _historySelected is null ? string.Empty : _gameRunning ? S["sm.block.GameRunning"]
        : HistoryCanCopyToAzimuth ? string.Empty : S["sm.block.NoSource"];
    public bool HasHistoryCopyBlocked => !string.IsNullOrEmpty(HistoryCopyBlocked);
    public string HistoryCloudCopyNote => _historySlot == SaveSlot.Azimuth ? string.Empty : S["sm.history_cloud_copy"];
    public bool HasHistoryCloudCopyNote => !string.IsNullOrEmpty(HistoryCloudCopyNote) && _historySelected is not null;
    public Brush HistoryAccent => SlotBrushes.Accent(_historySlot);

    public async Task OpenHistoryAsync(SaveSlot slot)
    {
        RefreshGameRunning();
        _historySlot = slot;
        SavesGo(SavesScreen.History);
        await FillHistoryAsync();
    }

    public Task ShowHistorySlotAsync(SaveSlot slot)
    {
        _historySlot = slot;
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
        HistoryRows.Clear();
        _historySelected = null;
        var rows = new List<(DateTimeOffset At, HistoryRow Row)>();
        var row = _historySlot == SaveSlot.Azimuth ? null : ProfileRows.FirstOrDefault(r => r.Card?.Player.Mode == (_historySlot == SaveSlot.Hardcore ? "hardcore" : "noob"));
        if (SafeHistory() is { } history)
        {
            foreach (var item in history.List())
            {
                var mine = _historySlot == SaveSlot.Azimuth ? item.Entry.Profile is null : item.Entry.Profile?.Xuid == row?.Profile.Xuid;
                if (!mine) continue;
                var at = DateTimeOffset.TryParse(item.Entry.At, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var w) ? w : DateTimeOffset.MinValue;
                var save = LocalHistory.NewestSave(item);
                var title = item.IsOriginal ? S["sm.original_name"]
                    : save?.Credits is { } cr ? S.Format("sm.stats_credits", ("credits", cr.ToString("N0", SavesCulture)))
                    : item.Entry.Empty == true ? S["sm.history_empty_entry"] : Size(item.Bytes);
                rows.Add((at, new HistoryRow(S["sm.origin.pc"], title, item.Entry.Reason.For(S.Locale), false) { Item = item, When = When(item.Entry.At) }));
            }
        }
        rows.AddRange(_cloudHistory.Where(_ => row is not null && _cloudHistoryXuid == row.Profile.Xuid).Select(CloudRow));
        if (row?.Card?.CareerRow is not null && _session is { } session && CloudSaveClient() is { } client && ParseXuid(row.Profile) is { } xuid && _cloudHistoryXuid != row.Profile.Xuid)
        {
            try
            {
                var access = await session.Tokens.AccessAsync(CancellationToken.None);
                var page = await client.Api.RevisionsAsync(access, row.Profile.Xuid, null, 50);
                if (page.Ok && page.Value is { } revisions)
                {
                    _cloudHistory.Clear();
                    _cloudHistory.AddRange(revisions.Items);
                    _cloudHistoryXuid = row.Profile.Xuid;
                    rows.AddRange(revisions.Items.Select(CloudRow));
                }
                else Log($"saves: cloud history of {row.Profile.ProfileId} → {page.Error?.Code ?? ((int)page.Status).ToString(CultureInfo.InvariantCulture)}");
            }
            catch (Exception e) when (e is HttpRequestException or ApiException or IOException or TaskCanceledException)
            {
                Log("saves: cloud history not read: " + e.GetType().Name);
            }
            _ = xuid;
        }
        foreach (var (_, r) in rows.OrderByDescending(r => r.At)) HistoryRows.Add(r);
        if (HistoryRows.Count > 0) SelectHistory(HistoryRows[0]);
        ChangedHistory();
    }

    private string? _cloudHistoryXuid;

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
        if (rev.Summary.ShownLevel() is { } level) parts.Add(S.Format("sm.stats_level", ("level", level.ToString("N0", c))));
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
            var row = ProfileRows.FirstOrDefault(p => p.Card?.Player.Mode == (_historySlot == SaveSlot.Hardcore ? "hardcore" : "noob"));
            if (row is null || ParseXuid(row.Profile) is not { } xuid) return;
            OpenCopy(new CopyPlan(_historySlot, name, null, null, rev, xuid, false));
        }
        else if (r.Item is { } item) OpenCopy(new CopyPlan(_historySlot, item.IsOriginal ? S["sm.original_name"] : name, null, item, null, null, item.IsOriginal));
    }

    // ------------------------------------------------------------------ Delete (§4), Done

    public string DeleteRateNote => S["sm.delete_note_rate"];

    /// <summary>
    /// §7.1 "Deleting is archiving": a deleted career's save on this PC goes into its own game's history before the cleanup moves it to the
    /// saves folder's archive. The server already deleted the career, so a failed copy is logged and the cleanup (which never deletes) goes on.
    /// </summary>
    private void ArchiveDeletedCareer(Infinity.Core.Process.GameDefinition game, Profile row, bool allGames)
    {
        if (_install is null || !ulong.TryParse(row.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)) return;
        var root = AzimuthSaves.SavesRoot(_install.Root, game);
        var folder = CloudSaveFiles.ProfileDir(root, xuid);
        if (!Directory.Exists(folder)) return;
        try
        {
            var history = new LocalHistory(_stateRoot, game, Version);
            history.EnsureOriginal(root, DateTime.UtcNow);
            var key = allGames ? "sm.reason.delete_profile" : "sm.reason.delete_career";
            var mode = row.Mode == "hardcore" ? "HARDCORE" : "NOOB";
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
            var states = HardcoreCheck.Steps(_seedOffer?.Result.Outcome, _busy);
            string[] keys = ["sm.check.read", "sm.check.progress", "sm.check.result"];
            return [.. states.Select((s, i) => new CheckStepRow(S[keys[i]], s))];
        }
    }

    public bool ShowCheckSteps => ShowSeedCheck || (ShowSeedChoice && _busy);

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
        Log($"saves: refused HARDCORE start, playing {source.ProfileId} as NOOB");
        ChooseProfile(source);
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
        Changed(nameof(HistoryNoobOn));
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
            nameof(ShowSavesResolve), nameof(ShowSavesManage), nameof(ShowSavesCopy), nameof(ShowSavesHistory), nameof(ShowSavesDelete),
            nameof(ShowSavesHelp), nameof(ShowSavesDone), nameof(ShowHelpLink), nameof(SavesCanGoBack), nameof(SavesTrail), nameof(SavesTrailLast),
            nameof(SavesSubtitle), nameof(SavesEmpty), nameof(AzimuthStripLine), nameof(ShowGameRunningNote), nameof(GameRunningText),
            nameof(CreateTitle), nameof(CreateAccent), nameof(ResolveCloud), nameof(ResolvePc), nameof(ResolveTitle), nameof(ResolveHardcore),
            nameof(CopyFromSide), nameof(CopyToSide), nameof(CopyTitle), nameof(CopySub), nameof(CopyNote), nameof(CanConfirmCopy),
            nameof(CopyBlockedText), nameof(ShowCopyBlocked), nameof(DoneText), nameof(ManageSub), nameof(HistorySizeText),
            nameof(ShowStartFromProfile), nameof(StartFromProfileLabel), nameof(StartFromProfileHint), nameof(StartFromProfileEnabled),
            nameof(StartFromPcEnabled), nameof(StartFromPcHint), nameof(StartFreshHint), nameof(StartAccent),
        }) Changed(name);
        ChangedHistory();
    }
}
