// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;

namespace Infinity.App;

/// <summary>What the PLAY notice line's one link does (<see cref="ShellViewModel.PlayNoticeItems"/>).</summary>
public enum PlayNoticeAction { None, QuitGame, OpenMods, RetryCatalog, ReviewCrash }

/// <summary>One message the PLAY notice line can show, with the link that belongs to it.</summary>
public sealed record PlayNoticeItem(string Text, PlayNoticeAction Action);

/// <summary>
/// Owner 2026-11-12 (first-hand, WINDOWS chat): PLAY holds only the logo, the game tiles, ONE notice line, the profile cards and "manage
/// profiles". Every message that used to sit around the PLAY buttons (the guidance, the mod notice and its buttons, the Play status line with
/// "Session ended" and the refusals, Close the game) is information on that one line: the most important first, never a stack of lines.
/// What needs the player's decision is a centred popup instead (the former-Noob choice, START CAREER, every save copy's confirmation).
/// Rank, top first: (1) a session under way (its live state; link: Close the game); (2) what stops PLAY now (the game not configured, a mod
/// state that blocks Play; link: Open Mods); (3) the last Play's outcome (session ended, refused, crashed, cancelled, kicked…; it stays
/// until the next Play replaces it; then a crash not handled yet, linking to its screen); (4) what the coming session will be like (modded session, mods a HARDCORE launch turns off, the mod
/// catalog loading or failed; link: Open Mods or Try again). The line shows the first; "+N more" (its tooltip lists them) says others wait.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The properties a named change of which redraws the notice line (a notify-all redraws it anyway).</summary>
    private static readonly HashSet<string> PlayNoticeInputs = new(StringComparer.Ordinal)
    {
        nameof(ModPlayNotice), nameof(ShowModPlayNotice), nameof(ShowCatalogRetry), nameof(HasCrashReport), nameof(PlayStatus), nameof(Playing),
        nameof(GameStarted), nameof(SetupNeeded), nameof(ModPlayBlocked), nameof(CanOpenMods), nameof(ModsIdle),
    };

    private static readonly string[] PlayNoticeOutputs =
    [
        nameof(PlayNotice), nameof(HasPlayNotice), nameof(PlayNoticeMore), nameof(HasPlayNoticeMore), nameof(PlayNoticeMoreTip),
        nameof(PlayNoticeActionLabel), nameof(HasPlayNoticeAction), nameof(PlayNoticeActionEnabled),
    ];

    /// <summary>Every message the notice line could show now, ranked (top first), each text once (read from the state, never cached).</summary>
    internal IReadOnlyList<PlayNoticeItem> PlayNoticeItems => ReadPlayNoticeItems();

    private List<PlayNoticeItem> ReadPlayNoticeItems()
    {
        var items = new List<PlayNoticeItem>();
        void Add(string? text, PlayNoticeAction action)
        {
            if (!string.IsNullOrWhiteSpace(text) && !items.Any(i => i.Text == text)) items.Add(new PlayNoticeItem(text, action));
        }
        // (1) a session under way: its live state ("Waiting for the game…", "Online", "Closing…")
        if (Playing) Add(_playStatus is { Length: > 0 } live ? live : S["play.session_active"], GameStarted ? PlayNoticeAction.QuitGame : PlayNoticeAction.None);
        var mod = ShowModPlayNotice ? ModPlayNotice : null;
        var modBlocks = mod is not null && (_modsLayoutFailure is not null || ModPlayBlocked);
        // (2) what stops PLAY now
        if (SetupNeeded) Add(S["play.setup_needed"], PlayNoticeAction.None);
        if (modBlocks) Add(mod, PlayNoticeAction.OpenMods);
        // (3) the last Play's outcome; an unhandled crash links to its screen (Get logs / Ignore are decided there)
        if (!Playing) Add(_playStatus, PlayNoticeAction.None);
        if (HasCrashReport && !Playing) Add(S.Format("play.crash_waiting", ("code", CrashCode)), PlayNoticeAction.ReviewCrash);
        // (4) the coming session
        if (mod is not null && !modBlocks) Add(mod, ShowCatalogRetry ? PlayNoticeAction.RetryCatalog : PlayNoticeAction.OpenMods);
        return items;
    }

    private PlayNoticeItem? TopPlayNotice => PlayNoticeItems is [var top, ..] ? top : null;

    /// <summary>The notice line: the most important message, or null (nothing to say: PLAY shows no line).</summary>
    public string? PlayNotice => TopPlayNotice?.Text;

    public bool HasPlayNotice => PlayNotice is not null;

    /// <summary>"+2 more" when other messages wait behind the shown one (their texts are its tooltip).</summary>
    public string PlayNoticeMore => PlayNoticeItems.Count is var n and > 1
        ? S.Format("play.notice_more", ("count", (n - 1).ToString(CultureInfo.CurrentCulture))) : string.Empty;

    public bool HasPlayNoticeMore => PlayNoticeItems.Count > 1;

    public string PlayNoticeMoreTip => string.Join("\n", PlayNoticeItems.Skip(1).Select(i => i.Text));

    private PlayNoticeAction PlayNoticeLink => TopPlayNotice?.Action ?? PlayNoticeAction.None;

    public string PlayNoticeActionLabel => PlayNoticeLink switch
    {
        PlayNoticeAction.QuitGame => S["supervisor.quit_button"],
        PlayNoticeAction.OpenMods => S["mods.open"],
        PlayNoticeAction.RetryCatalog => S["updater.retry"],
        PlayNoticeAction.ReviewCrash => S["crash.review"],
        _ => string.Empty,
    };

    public bool HasPlayNoticeAction => PlayNoticeLink != PlayNoticeAction.None;

    public bool PlayNoticeActionEnabled => PlayNoticeLink switch
    {
        PlayNoticeAction.OpenMods => CanOpenMods,
        PlayNoticeAction.RetryCatalog => ModsIdle,
        _ => true,
    };

    /// <summary>The notice line's link: Close the game, Open Mods or the catalog's Try again (what the inline buttons did).</summary>
    internal async Task RunPlayNoticeActionAsync()
    {
        switch (PlayNoticeLink)
        {
            case PlayNoticeAction.QuitGame: QuitGame(); break;
            case PlayNoticeAction.OpenMods: if (CanOpenMods) OpenMods(); break;
            case PlayNoticeAction.RetryCatalog: if (ModsIdle) await LoadModCatalogAsync(); break;
            case PlayNoticeAction.ReviewCrash: ReviewCrash(); break;
        }
    }
}
