// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Mods;
using Infinity.Core.Shell;

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
    public bool ShowQueueModal => _waitingForAdmission && _queueShown && _playOverlay is null;

    /// <summary>The Play's overlay with <paramref name="label"/> (opened, or its label changed).</summary>
    private void ShowPlayOverlay(string label)
    {
        if (_playOverlay is { } open) open.Step(label);
        else _playOverlay = Loading(label);
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
        Changed(nameof(ShowQueueModal));
        Changed(nameof(CanCancelBusy));
    }

    /// <summary>The busy overlay's Cancel: a sign-in waiting on the network, or a Play still asking for its place (Leave queue).</summary>
    public void CancelBusy()
    {
        if (_signIn is not null) { CancelSignIn(); return; }
        if (_playOverlay is not null && CanLeaveQueue) LeaveQueue();
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
