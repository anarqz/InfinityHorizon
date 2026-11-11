// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Mods;
using Infinity.Core.Shell;
using Infinity.Core.Supervisor;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10 (3.1.1): "When clicking to play, there's absolutely no feedback." From the click on, a Play always shows where it is:
/// the centre overlay "Joining queue…" through the launch preparation and the admission request; the queue modal (blocking the screen
/// like the overlay, its Leave queue button still working) once the server says where the player is; "Joining game…" from the admission
/// until the game has started (the hook reports in, or the supervisor waits for it with the game open). An offline Play shows
/// "Starting the game…" until the game runs. Every failure or cancel closes them and says why (Message / PlayStatus).
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The Play's centre overlay ("Joining queue…", "Joining game…", "Starting the game…"), or null.</summary>
    private BusyState.BusyScope? _playOverlay;

    /// <summary>The server has said where the player is (queued, or a retry): the queue modal shows until the admission ends.</summary>
    private bool _queueShown;

    /// <summary>The queue modal: shown over the whole window, which takes no click or key outside it (its own buttons still do).</summary>
    public bool ShowQueueModal => _waitingForAdmission && _queueShown && _playOverlay is null && !AwaitingPlayer
        && _playAttempt is { IsCancellationRequested: false };

    // ------------------------------------------------------------------ hotfix 3.1.2: a Play never hides a question, never a dead end

    /// <summary>
    /// Hotfix 3.1.2 (players stuck under "Joining game…" with the cloud/PC save choice behind it): a screen is open that waits for the
    /// player's answer. Every decision prompt the launcher has is named here, so a Play's overlay and its queue modal step aside for any
    /// of them (<see cref="PlayStepsAside"/>): the save conflict (Resolve: cloud or this PC), the refused first save (OD-133), a restored
    /// career waiting for the cloud and the restore itself, the career start (CR-S52 seed / scratch), a custom server's changed keys
    /// (CR-L39), an Azimuth never opened, a mod confirmation and the sign-in form (OD-240: old Noob saves never prompt; Settings lists them).
    /// A new prompt is added here, nowhere else.
    /// </summary>
    public bool AwaitingPlayer => ShowSaveConflict || ShowFreshStartRefused || ShowRestoreWaiting || ShowCareerRestore || ShowSeedPanel
        || ShowTrustChange || ShowAzimuthFirstRun || ShowModConfirm || ShowSignIn;

    /// <summary>
    /// A Play is under way and a prompt waits for the player: the Play's overlay is hidden (its scope stays open with its label, so it
    /// comes back as it was once the answer is given and the launch goes on) and the screen is not locked, so the prompt takes the mouse
    /// and the keyboard. The queue modal is hidden the same way (<see cref="ShowQueueModal"/>).
    /// </summary>
    private bool PlayStepsAside => _playOverlay is not null && AwaitingPlayer;

    /// <summary>When the Play's overlay opened (its Cancel shows <see cref="PlayCancelAfter"/> later).</summary>
    private long _playOverlaySince;

    /// <summary>Hotfix 3.1.2: the Play's overlay offers Cancel this long after it opened (a harness seam).</summary>
    internal TimeSpan PlayCancelAfter { get; set; } = TimeSpan.FromSeconds(20);

    /// <summary>The Play's overlay has been up long enough to offer Cancel.</summary>
    private bool PlayCancelReady => _playOverlay is not null && System.Diagnostics.Stopwatch.GetElapsedTime(_playOverlaySince) >= PlayCancelAfter;

    /// <summary>The running Play attempt's own cancel (the preparation before the supervisor runs); null when none runs.</summary>
    private CancellationTokenSource? _playAttempt;

    /// <summary>The supervisor started the game process for the current attempt (a cancel then closes the game instead of leaving the gate).</summary>
    private bool _playSpawned;

    /// <summary>The running online attempt's supervisor (review 3.1.2, finding 5), or null.</summary>
    private PlaySupervisor? _supervisor;

    /// <summary>
    /// One Play attempt (online or offline) under its overlay: the overlay opens with <paramref name="label"/>, and whatever ends the attempt
    /// (the game started, a refusal, a cancel, the session's end, an exception) closes it. A cancelled attempt that is still unwinding
    /// refuses a second Play until it has stopped.
    /// </summary>
    private async Task RunPlayAttemptAsync(string label, Func<CancellationToken, Task> attempt)
    {
        if (_playAttempt is { IsCancellationRequested: true }) { Message = S["play.cancelled"]; return; }
        using var own = new CancellationTokenSource();
        _playAttempt = own;
        _playSpawned = _playLaunched = false;
        ShowPlayOverlay(label);
        try { await attempt(own.Token); }
        finally
        {
            if (ReferenceEquals(_playAttempt, own)) _playAttempt = null;
            EndPlayProgress();
            if (!_playLaunched) PlayRefusedVisibly();
            Changed(string.Empty); // the mods page's buttons (ModsIdle) come back once a cancelled attempt has stopped
        }
    }

    /// <summary>The attempt reached the supervisor (online) or started the game (offline): its own events say how it ended.</summary>
    private bool _playLaunched;

    /// <summary>
    /// Hotfix 3.1.2 (PLAY HARDCORE clicked 14 times, "no feedback"): a Play that stopped before the game was launched always says why where
    /// the player looks, PLAY's own status line under the buttons (not only the window's bottom line), and the launcher log has it. A
    /// prompt that opened instead (the career start, a conflict) is its own answer.
    /// </summary>
    private void PlayRefusedVisibly()
    {
        if (AwaitingPlayer) return;
        if (string.IsNullOrEmpty(Message)) PlayStopped();
        Log("play: not started: " + Message);
        SetStatus(Message!);
    }

    /// <summary>
    /// Hotfix 3.1.2, the overlay's Cancel (shown <see cref="PlayCancelAfter"/> after it opened): the launch stops and the overlay closes at
    /// once, with the reason. Before the supervisor runs, the preparation stops at its next step; while the gate is asked or the game is
    /// prepared (its save, a prompt), the supervisor is cancelled and leaves the gate (as Leave queue); once the game process exists, it is
    /// closed as "Close the game" does.
    /// </summary>
    internal void CancelPlay()
    {
        if (_playOverlay is null && !ShowQueueModal) return;
        Log("play: cancelled by the player from the Play overlay" + (_playSpawned ? " (the game is closed)" : string.Empty));
        try { _playAttempt?.Cancel(); }
        catch (ObjectDisposedException) { } // already over
        if (_play is not null && _offlineChild is null && _commands is not null)
        {
            // review 3.1.2 (finding 5): once the supervisor runs, a cancel never leaves a started game without its session. Still asking
            // the gate (nothing written yet): the attempt stops as Leave queue does. Admitted: the preparation stops at its next checkpoint,
            // never inside a cloud save step (CancelBeforeSpawn), and a game that was started meanwhile is closed (Quit), whether or not the
            // Spawned event has reached the window yet.
            if (_waitingForAdmission && !_playSpawned) StopPlay();
            else
            {
                _supervisor?.CancelBeforeSpawn();
                _commands.Writer.TryWrite(new SupervisorCommand.Quit());
            }
        }
        EndPlayProgress();
        Message = S["play.cancelled"];
        SetStatus(Message);
    }

    /// <summary>The Play's overlay with <paramref name="label"/> (opened, or its label changed).</summary>
    private void ShowPlayOverlay(string label)
    {
        // hotfix 3.1.2: only for a Play attempt still going (a late supervisor event after a cancel never reopens it)
        if (_playAttempt is not { IsCancellationRequested: false }) return;
        if (_playOverlay is { } open) open.Step(label);
        else { _playOverlay = Loading(label); _playOverlaySince = System.Diagnostics.Stopwatch.GetTimestamp(); }
        ChangedPlayProgress();
    }

    /// <summary>Closes the Play's overlay (nothing when none is open).</summary>
    private void HidePlayOverlay()
    {
        var open = _playOverlay;
        _playOverlay = null;
        open?.Dispose();
        ChangedPlayProgress();
    }

    /// <summary>The overlay and the queue modal both closed: the Play ended, failed or was cancelled.</summary>
    private void EndPlayProgress()
    {
        _queueShown = false;
        HidePlayOverlay();
    }

    private void ChangedPlayProgress()
    {
        Changed(nameof(ShowBusyOverlay));
        Changed(nameof(ShowQueueModal));
        Changed(nameof(CanCancelBusy));
    }

    /// <summary>
    /// The busy overlay's Cancel: a sign-in waiting on the network, a Play still asking for its place (Leave queue), or (hotfix 3.1.2) any
    /// Play whose overlay has been up for <see cref="PlayCancelAfter"/>.
    /// </summary>
    public void CancelBusy()
    {
        if (_signIn is not null) { CancelSignIn(); return; }
        if (_playOverlay is null) return;
        if (CanLeaveQueue) LeaveQueue();
        else if (PlayCancelReady) CancelPlay();
    }

    // ------------------------------------------------------------------ the mods a launch needs (owner 2026-11-10, 3.1.1)

    /// <summary>Set while a launch reads its game's mods itself: that read shows no toast (the Play's overlay is up).</summary>
    private bool _modReadQuiet;

    /// <summary>
    /// Owner 2026-11-10 ("remove the 'read mods' when clicking on PLAY"): a launch uses the mod state already read for its game (the
    /// start-up read, a game switch, the page's own reads), waiting for one that runs. Only when none was read for this game yet does it
    /// read it, under the Play's overlay and without a toast; the files' hashes come from the kept hashes (path, length, write time).
    /// </summary>
    private async Task EnsureModStateForLaunchAsync()
    {
        if (_install is null || !ExtraOptionsLayout.HasModsFolder(CurrentGame.Id) || ModManagerFactory() is null) return;
        var game = CurrentGame.Id;
        if (_modReadRunning && _modReadLoop is { } running)
        {
            try { await running; } catch (Exception error) when (error is not OutOfMemoryException) { Log("mods: read before launch: " + ErrorChain(error)); }
        }
        if (ModReadsApplied > 0 && _modStateGame == game && ReferenceEquals(_modFactsCatalog, _manifestCatalog)) return;
        _modReadQuiet = true;
        try { await RefreshModsAsync(); }
        finally { _modReadQuiet = false; }
    }

    // ------------------------------------------------------------------ the mod state per game (owner 2026-11-10, 3.1.1)

    /// <summary>One game's mod state as its last read left it, and the bucket's stamp then.</summary>
    private sealed record KeptModState(ModHealth Health, IReadOnlyList<ModCardFacts> Facts, IReadOnlyList<ModManifest> Catalog, string Stamp);

    /// <summary>
    /// Owner 2026-11-10 ("changing games should only read the mods from that game folder"): every game's last read, so switching back
    /// shows it without reading again while the bucket's receipts did not change.
    /// </summary>
    private readonly Dictionary<string, KeptModState> _modStateByGame = new(StringComparer.Ordinal);

    /// <summary>The game the applied mod state (<see cref="_modHealth"/>, <see cref="_modFacts"/>) was read for.</summary>
    private string? _modStateGame;

    /// <summary>What the stamp is taken of: the Azimuth folder and the game folder the manager reads.</summary>
    private string ModStateStampKey => (_install?.Root ?? "") + "|" + (ContentLocation?.Mount ?? "");

    /// <summary>
    /// A cheap stamp of the bucket (no hashing): its records' count, sizes and last write. Any install, change, removal or recovery
    /// rewrites a record there, so a kept state with the same stamp is still what a read would find. Never throws (unreadable: unique).
    /// </summary>
    private static string ModStateStamp(Func<ManagedModFiles>? factory, string key)
    {
        if (factory is null) return key + "|none";
        try
        {
            var root = factory().BackupRoot;
            if (!Directory.Exists(root)) return key + "|empty";
            long count = 0, size = 0, last = 0;
            foreach (var file in new DirectoryInfo(root).EnumerateFiles())
            {
                count++; size += file.Length;
                last = Math.Max(last, file.LastWriteTimeUtc.Ticks);
            }
            return key + "|" + count + "|" + size + "|" + last;
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return key + "|" + Guid.NewGuid().ToString("N"); }
    }

    /// <summary>
    /// A game switch: the chosen game's kept state at once (its cards drawn from memory), checked against the bucket's stamp off the UI
    /// thread; read again (only that game) when nothing is kept or something changed.
    /// </summary>
    private async Task ShowModStateForGameAsync()
    {
        var game = CurrentGame.Id;
        if (_modStateByGame.TryGetValue(game, out var kept) && ReferenceEquals(kept.Catalog, _manifestCatalog))
        {
            ApplyKeptModState(game, kept);
            var factory = ModManagerFactory(); var key = ModStateStampKey;
            var stamp = await Task.Run(() => ModStateStamp(factory, key));
            if (CurrentGame.Id != game) return; // another switch: its own check
            if (stamp == kept.Stamp && _modStateByGame.TryGetValue(game, out var still) && ReferenceEquals(still, kept))
            {
                Log("mods: " + game + " shown from its last read (unchanged)");
                return;
            }
        }
        await RefreshModsAsync();
    }

    private void ApplyKeptModState(string game, KeptModState kept)
    {
        _modHealth = kept.Health; _modFacts = kept.Facts; _modFactsCatalog = kept.Catalog; _modStateError = null; _modStateGame = game;
        try
        {
            RefreshModCards();
            _ = GuardAsync("list the mods folder", RefreshExtraOptionsModsAsync); // the folder's listing (no hashing)
        }
        finally { Changed(string.Empty); }
    }
}
