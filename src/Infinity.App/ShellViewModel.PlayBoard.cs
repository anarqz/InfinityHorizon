// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Windows.Media;
using Infinity.Core.Api;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// One of PLAY's two large buttons (owner 2026-11-08): the profile's career in the selected game ("LV 15 · CR 15,858 · 22h") above
/// "PLAY HARDCORE" / "PLAY NOOB", or what the button does instead (start the career, create the profile, resolve, not offered).
/// </summary>
public sealed record PlayButtonCard(string Mode, PlayButtonKind Kind, string ModeLabel, string Title, string Gamertag, string Stats, string Note)
{
    public SaveSlot Slot => Mode == "hardcore" ? SaveSlot.Hardcore : SaveSlot.Noob;
    public Brush Accent => SlotBrushes.Accent(Slot);
    public Brush Soft => SlotBrushes.Soft(Slot);
    public bool Enabled { get; init; }
    public bool HasGamertag => Gamertag.Length > 0;
    public bool HasStats => Stats.Length > 0;
    public bool HasNote => Note.Length > 0;
    public string Initial => Gamertag.Length > 0 ? Gamertag[..1].ToUpperInvariant() : "+";
    public bool IsPlay => Kind == PlayButtonKind.Play;
    internal ProfileRow? Row { get; init; }
}

/// <summary>
/// Owner 2026-11-08, the PLAY revamp: a big title, the catalog's game tiles (selecting one is instant: no request, no spinner; the last one is
/// remembered), two buttons from the one <c>/me</c> (<see cref="PlayBoard"/>), "manage profiles", and the FRIENDS column. Clicking PLAY
/// {MODE} selects that profile and game and launches through the existing play path (its gates unchanged); "START {MODE} CAREER" opens the
/// inline choice (fresh / from my other profile / from this PC's save) and Play follows once the career is chosen.
/// </summary>
public sealed partial class ShellViewModel
{
    public ObservableCollection<PlayButtonCard> PlayButtons { get; } = [];

    /// <summary>A career started from PLAY's inline choice: Play follows as soon as it is chosen.</summary>
    private bool _playAfterStart;

    public string PlayTitle => S["play.title"];

    /// <summary>The two buttons take clicks (never during the busy overlay or while a game session runs).</summary>
    public bool CanUsePlayButtons => Idle && !Playing;

    /// <summary>The inline start of PLAY (the Start choices for the card of <see cref="_startRow"/>).</summary>
    public bool ShowPlayStart => _startRow is not null && ShowHome;

    /// <summary>The buttons and tiles, rebuilt from <see cref="ProfileRows"/> (no request): every /me, game and gate change.</summary>
    private void FillPlayBoard()
    {
        FillGameCards();
        PlayButtons.Clear();
        var game = CurrentGame;
        var cards = ProfileRows.Where(r => r.Card is not null).Select(r => r.Card!).ToList();
        IReadOnlyList<PlaySlot> slots = cards.Count > 0 || (!OfflineMode && _session?.Me?.Players is not null)
            ? PlayBoard.Build(cards, game, card => ProfileRows.FirstOrDefault(r => ReferenceEquals(r.Card, card)) is { } row && SaveChips.NeedsResolve(ChipOf(row)))
            : PlayBoard.BuildPlain(ProfileRows.Select(r => r.Profile.Mode).ToHashSet(StringComparer.Ordinal), game);
        foreach (var slot in slots) PlayButtons.Add(ButtonOf(slot));
        Changed(nameof(ShowPlayStart));
        Changed(nameof(PlayTitle));
    }

    private PlayButtonCard ButtonOf(PlaySlot slot)
    {
        var slotKind = slot.Mode == "hardcore" ? SaveSlot.Hardcore : SaveSlot.Noob;
        var mode = ModeWord(slotKind);
        var row = slot.Card is { } card ? ProfileRows.FirstOrDefault(r => ReferenceEquals(r.Card, card))
            : ProfileRows.FirstOrDefault(r => r.Profile.Mode == slot.Mode);
        var gamertag = slot.Card?.Player.Gamertag is { } tag ? ProfileName.Plain(tag) : row?.Gamertag ?? string.Empty;
        var (title, note) = slot.Kind switch
        {
            PlayButtonKind.Play => (S.Format("play.button_play", ("mode", mode)), string.Empty),
            PlayButtonKind.StartCareer => (S.Format("play.button_start", ("mode", mode)), S.Format("play.no_career", ("game", GameLabel))),
            PlayButtonKind.CreateProfile => (S.Format("play.button_create", ("mode", mode)), S.Format(OfflineMode && slot.Mode == "hardcore" ? "play.create_hint_signin" : "play.create_hint", ("mode", mode))),
            PlayButtonKind.Resolve => (S["play.button_resolve"], S["play.resolve_hint"]),
            _ => (S.Format("play.button_play", ("mode", mode)), S.Format("play.not_offered", ("mode", mode), ("game", GameLabel))),
        };
        var stats = slot.Kind is PlayButtonKind.Play or PlayButtonKind.Resolve
            ? slot.Card is { } started ? PlayStats(started.Career) : S["play.stats_local"]
            : string.Empty;
        var (enabled, reason) = PlayButtonEnabled(slot.Kind);
        return new PlayButtonCard(slot.Mode, slot.Kind, mode, title, gamertag, stats, reason ?? note) { Enabled = enabled, Row = row };
    }

    /// <summary>"LV 15 · CR 15,858 · 22h" from the career's progress; absent progress is "No progress yet" (level 0 is real).</summary>
    private string PlayStats(Career? career)
    {
        if (career?.Progress is not { } p) return S["play.stats_none"];
        var c = SavesCulture;
        var parts = new List<string>();
        if (p.Level is { } level) parts.Add(S.Format("play.stats_level", ("level", level.ToString(c))));
        if (p.Credits is { } credits) parts.Add(S.Format("play.stats_credits", ("credits", credits.ToString("N0", c))));
        if (p.PlaySeconds is { } seconds) parts.Add(S.Format("play.stats_hours", ("hours", (seconds / 3600).ToString(c))));
        return parts.Count == 0 ? S["play.stats_none"] : string.Join("  ·  ", parts);
    }

    /// <summary>
    /// (5) The gates PLAY can tell before a click (a game running, the busy overlay, no Azimuth); everything else (integrity, the release
    /// floor, online paused, mods) is the play path's own check, which says why in the status line.
    /// </summary>
    private (bool Enabled, string? Reason) PlayButtonEnabled(PlayButtonKind kind)
    {
        if (kind == PlayButtonKind.NotOffered) return (false, null);
        if (kind is PlayButtonKind.CreateProfile or PlayButtonKind.Resolve) return (!_busy && !Playing, Playing ? S["play.session_active"] : null);
        if (Playing) return (false, S["play.session_active"]);
        if (_install is null) return (false, S["play.setup_needed"]);
        return (!_busy, null);
    }

    /// <summary>A game tile.</summary>
    public void SelectPlayGame(GameCard card)
    {
        ArgumentNullException.ThrowIfNull(card);
        if (!card.Enabled) return;
        ChooseGame(card.Info);
        FillPlayBoard();
        Changed(string.Empty);
    }

    /// <summary>One of the two buttons.</summary>
    public async Task PlayButtonAsync(PlayButtonCard button)
    {
        ArgumentNullException.ThrowIfNull(button);
        if (!button.Enabled || _busy) return;
        Message = null;
        switch (button.Kind)
        {
            case PlayButtonKind.Play when button.Row is { } row:
                _startRow = null;
                ChooseProfile(row.Profile);
                Log($"play: {button.Mode} {CurrentGame.Id} chosen on PLAY");
                await PlayAsync();
                break;
            case PlayButtonKind.StartCareer when button.Row is { } row:
                _playAfterStart = true;
                PickProfileRow(row); // opens the inline choice (a card that cannot start says why)
                break;
            case PlayButtonKind.Resolve when button.Row is { } row:
                OpenProfiles();
                OpenResolve(row);
                break;
            case PlayButtonKind.CreateProfile:
                OpenProfiles();
                if (ShowSaveManager)
                {
                    _createMode = button.Mode;
                    NewHardcore = button.Mode == "hardcore";
                    SavesGo(SavesScreen.Create);
                }
                break;
        }
        FillPlayBoard();
        Changed(string.Empty);
    }

    /// <summary>PLAY's "manage profiles" link.</summary>
    public void ManageProfiles() => OpenProfiles();

    /// <summary>The inline choice's "Not now".</summary>
    public void ClosePlayStart()
    {
        _playAfterStart = false;
        CancelCareerStart();
        FillPlayBoard();
    }

    /// <summary>The inline choice's three ways; each ends with the career chosen, then Play follows.</summary>
    public async Task PlayStartFreshAsync() { await StartCareerFreshAsync(); await PlayAfterStartAsync(); }

    public async Task PlayStartFromProfileAsync() { await StartCareerFromProfileAsync(); await PlayAfterStartAsync(); }

    public async Task PlayStartFromPcAsync() { await StartCareerFromExistingAsync(); await PlayAfterStartAsync(); }

    /// <summary>
    /// Play follows a start once its career is chosen and nothing else asks the player first (the HARDCORE check's panel or this PC's save
    /// picker end by choosing it; then the player's PLAY button launches).
    /// </summary>
    private async Task PlayAfterStartAsync()
    {
        FillPlayBoard();
        if (!_playAfterStart) return;
        for (var waited = 0; _busy && waited < 150; waited++) await Task.Delay(200);
        if (_startRow is not null || ShowSeedPanel || ShowPickSave) return; // still choosing, or another panel asks first
        _playAfterStart = false;
        if (!Ready || !ShowHome || Playing) return;
        Log($"play: career started on PLAY, playing {CurrentGame.Id}");
        await PlayAsync();
    }
}
