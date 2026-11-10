// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.IO;
using Infinity.Core.Process;
using Infinity.Core.Saves;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10 ("our launcher must be fluid"): this PC's save facts behind PLAY's cards, the Profiles rows and their chips. A sealed
/// save is found by hashing every file of every revision, and one refresh of the cards asked for it dozens of times on the UI thread
/// (the spinner then stood still). Every read now goes through <see cref="_saveScan"/> (stamped by a folder listing), and
/// <see cref="PrimeLocalFactsAsync"/> computes what changed off the UI thread, inside the busy overlay, before the cards are drawn.
/// </summary>
public sealed partial class ShellViewModel
{
    // owner 2026-11-10: the UI thread trusts the last off-thread scan (PrimeLocalFactsAsync re-checks every answer on its worker)
    private readonly SaveScanCache _saveScan = new() { TrustWithinEpoch = true };

    /// <summary>The Azimuth folders whose saves history original was kept this run (once per folder and game, off the UI thread).</summary>
    private readonly HashSet<string> _originalKept = new(StringComparer.OrdinalIgnoreCase);

    /// <summary>The cached scan of <paramref name="savesRoot"/> (<see cref="AzimuthSaves.List"/>).</summary>
    private IReadOnlyList<SaveProfile> SavesList(string savesRoot, string? exclude = null)
    {
        using var span = Span();
        return _saveScan.List(savesRoot, exclude);
    }

    /// <summary>The cached newest sealed revision of one profile folder (<see cref="AzimuthSaves.NewestSealed"/>).</summary>
    private LocalRevision? NewestSealedOf(string profileDir)
    {
        using var span = Span();
        return _saveScan.NewestSealed(profileDir);
    }

    /// <summary>The cached content digest of <paramref name="revision"/> in <paramref name="game"/>'s bundle format.</summary>
    private string ContentDigestOf(GameDefinition game, LocalRevision revision)
    {
        using var span = Span();
        return _saveScan.Digest(revision, game.Id, game.Saves.Bundle.ContentDigest);
    }

    /// <summary>
    /// Off the UI thread: every game's saves of this Azimuth folder scanned (lists, newest revisions, content digests), whether
    /// azimuth.exe runs, and the history original of the current game kept once. Cheap when nothing changed (folder listings only).
    /// </summary>
    internal async Task PrimeLocalFactsAsync()
    {
        if (PrimeLocalFactsForTests is { } fake) { await fake(); return; }
        // no Invalidate here: the worker re-checks every known answer (Prime), and the UI thread keeps the previous ones meanwhile
        var root = _install?.Root;
        var current = CurrentGame;
        var keepOriginal = root is not null && _session is not null && !OfflineMode && _originalKept.Add(root + "|" + current.Id);
        var history = keepOriginal ? History() : null;
        var watch = Stopwatch.StartNew();
        var (changed, running) = await Task.Run(() =>
        {
            var changed = root is not null && _saveScan.Prime(GameDefinition.All.Select(game =>
                (AzimuthSaves.SavesRoot(root, game), game.CloudSaves ? game.Id : null, game.CloudSaves ? (Func<LocalRevision, string>)game.Saves.Bundle.ContentDigest : null)));
            if (history is not null) KeepOriginal(history, AzimuthSaves.SavesRoot(root!, current), current);
            return (changed, AzimuthRunning());
        }).ConfigureAwait(true);
        _azimuthRunning = running;
        _gameRunning = SavesForceRunning || Playing || running;
        if (changed) Log($"saves: this PC's saves scanned off the UI thread in {watch.ElapsedMilliseconds} ms");
    }

    /// <summary>Tests only: replaces the scan (a check makes it throw what a broken saves folder throws).</summary>
    internal Func<Task>? PrimeLocalFactsForTests { get; set; }

    /// <summary>
    /// <see cref="PrimeLocalFactsAsync"/> that never throws (an unreadable folder is reported by the read that needs it). Every error but a
    /// fatal one is logged and skipped: a <see cref="SaveException"/> (a folder whose revision counter is exhausted) or any other must never
    /// leave the overlay of the action that awaited it up.
    /// </summary>
    private async Task PrimeLocalFactsQuietlyAsync()
    {
        try { await PrimeLocalFactsAsync(); }
        catch (Exception e) when (!IsFatal(e)) { Log("saves: scan skipped: " + e.GetType().Name + ": " + e.Message); }
    }

    /// <summary>An exception nothing should try to carry on from (the process is in trouble): never swallowed.</summary>
    internal static bool IsFatal(Exception e) => e is OutOfMemoryException or StackOverflowException or AccessViolationException
        or System.Runtime.InteropServices.SEHException;

    /// <summary>
    /// Owner 2026-11-10 review: an action that copies, clones, adopts or archives decides from this PC's saves as they are now, never from
    /// the cards' last scan (the UI thread trusts it, <see cref="SaveScanCache.TrustWithinEpoch"/>): the scan re-checked off the UI thread,
    /// then the list read on a worker. Called under the action's own overlay.
    /// </summary>
    private async Task<IReadOnlyList<SaveProfile>> FreshSavesListAsync(string savesRoot, string? exclude = null)
    {
        await PrimeLocalFactsQuietlyAsync();
        return await Task.Run(() => _saveScan.List(savesRoot, exclude));
    }

    /// <summary>azimuth.exe was running at the last off-thread scan.</summary>
    private bool _azimuthRunning;

    /// <summary>azimuth.exe runs (a process list: off the UI thread).</summary>
    private static bool AzimuthRunning()
    {
        try
        {
            var found = Process.GetProcessesByName("azimuth");
            foreach (var process in found) process.Dispose();
            return found.Length > 0;
        }
        catch (InvalidOperationException) { return false; }
    }

    /// <summary>§7.1's original of the saves folder (a copy on the first time): on a worker, logged either way.</summary>
    private void KeepOriginal(LocalHistory history, string savesRoot, GameDefinition game)
    {
        try
        {
            if (history.EnsureOriginal(savesRoot, DateTime.UtcNow) is { } original)
                Log($"saves: history original of {game.Id}/{game.TitleKey} kept ({original.Entry.Files.Count} files)");
        }
        catch (Exception e) when (e is HistoryException or IOException or UnauthorizedAccessException) { Log("saves: history original not kept: " + e.Message); }
    }

    /// <summary>
    /// The window came back to the front: the player may have played or copied saves outside the launcher. Scanned off the UI thread;
    /// only a change rebuilds the cards (no overlay for a background look that finds nothing new).
    /// </summary>
    internal async Task RefreshLocalFactsQuietlyAsync()
    {
        if (!InShell || _busy || _busyState.IsBusy || Playing || _install is null) return;
        var before = _saveScan.Computations;
        var wasRunning = _gameRunning;
        await PrimeLocalFactsQuietlyAsync(); // the window's Activated handler: an error is logged, never thrown into the dispatcher
        if (_saveScan.Computations == before && wasRunning == _gameRunning) return;
        if (_busy || _busyState.IsBusy) return; // an action started meanwhile: its own end rebuilds the screen
        FillProfiles();
        Changed(string.Empty);
    }
}
