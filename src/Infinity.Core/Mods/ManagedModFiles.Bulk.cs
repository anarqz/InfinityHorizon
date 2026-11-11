// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Mods;

/// <summary>A mod <see cref="ManagedModFiles.SetEnabledAll"/> left as it was, and why (a <see cref="ModErrors"/> code).</summary>
public sealed record ModToggleFailure(string Id, string Code);

/// <summary>
/// What <see cref="ManagedModFiles.SetEnabledAll"/> did: the managed mods it switched, those it could not (each with its reason), and how
/// many lines of hand-copied folders it switched in <c>loadorder.txt</c> (<see cref="ManualFolders"/>: which).
/// </summary>
public sealed record ModToggleAllResult(IReadOnlyList<string> Changed, IReadOnlyList<ModToggleFailure> Failed, int ManualChanged)
{
    public IReadOnlyList<string> ManualFolders { get; init; } = [];
}

/// <summary>What <see cref="ManagedModFiles.HoldForHardcore"/> did: the mods it turned off (or back on), and one log line per change.</summary>
public sealed record ModHoldResult(IReadOnlyList<string> Changed, IReadOnlyList<string> Notes)
{
    public static ModHoldResult None { get; } = new([], []);
}

public sealed partial class ManagedModFiles
{
    /// <summary>While <see cref="SetEnabledAll"/> runs: the installed snapshots read once, and the digests whose files were checked.</summary>
    private sealed record BulkState(ModManifest[] Snapshots, IReadOnlyList<string> Unreadable, HashSet<string> Verified);

    private BulkState? _bulk;

    /// <summary>
    /// Review 3.1.2: where the launcher keeps, per game, the managed mods a Hardcore launch held off (<see cref="HoldForHardcore"/>'s file)
    /// and the folders the hand-copied hold turned off (<see cref="ExtraOptionsMods.PrepareForLaunch"/>'s). A mod the player enables,
    /// disables or uninstalls here leaves both, so no later launch gives back what they turned off themselves. Null: not kept.
    /// </summary>
    public Func<string, string>? HardcoreHeldFile { get; init; }

    /// <inheritdoc cref="HardcoreHeldFile"/>
    public Func<string, string>? HandHeldFile { get; init; }

    /// <summary>The player's own change of <paramref name="manifest"/>: it leaves both holds of its game (the caller holds the lock).</summary>
    private void ForgetHeldLocked(ModManifest manifest)
    {
        try
        {
            if (HardcoreHeldFile?.Invoke(manifest.ModsGame) is { } managed) ForgetHeldIds(managed, [manifest.Id]);
            if (HandHeldFile?.Invoke(manifest.ModsGame) is { } hand && ExtraOptionsMods.FolderOf(manifest) is { } folder) ExtraOptionsMods.ForgetHeld(hand, [folder]);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { } // the operation itself is done; a later launch retries nothing wrong
    }

    /// <summary>A dependency's files are hashed once per <see cref="SetEnabledAll"/> (a Common under 43 packs), every time otherwise.</summary>
    private bool VerifiedOnce(ModManifest manifest)
    {
        if (_bulk is { } bulk && bulk.Verified.Contains(manifest.Digest)) return true;
        var verified = Verified(manifest);
        if (verified) _bulk?.Verified.Add(manifest.Digest);
        return verified;
    }

    /// <summary>The errors a bulk operation stops for as a whole (the next attempt decides); any other skips one mod.</summary>
    private static bool Stops(Exception error) => error is ModException { Code: ModErrors.Recovery or ModErrors.Busy or ModErrors.GameRunning };

    /// <summary>
    /// Review 3.1.2: a receipt read inside a bulk loop: an unreadable or mismatched one (a damaged file, another manifest) is that mod's
    /// problem only (null, with a note), never the whole operation's.
    /// </summary>
    private ManagedModReceipt? ReceiptOrNull(ModManifest manifest, List<string>? notes)
    {
        try { return Receipt(manifest); }
        catch (Exception error) when (IsStateError(error) && !Stops(error)) { notes?.Add("mods: " + manifest.Id + " skipped (" + Code(error) + ")"); return null; }
    }

    /// <summary>The follow-up steps every bulk operation ends with, whatever stopped it: the Wine overrides and the load-first mods on top.</summary>
    private void FinishBulk(List<string>? notes, IEnumerable<string>? loadFirst = null, string? game = null)
    {
        try
        {
            ReconcileWineOverrides();
            var first = (loadFirst ?? []).ToArray();
            if (first.Length > 0 && game is not null) ExtraOptionsMods.ArrangeLoadOrder(_azimuth, first, game);
            else KeepLoadFirstOnTop();
        }
        catch (Exception error) when (IsStateError(error)) { notes?.Add("mods: follow-up steps not finished (" + Code(error) + ")"); }
    }

    /// <summary>
    /// Alex 2026-11-10: "Enable all" / "Disable all" for one game's mods, in one operation under one lock. Every managed mod of the game
    /// goes through the single-mod path (<see cref="SetEnabled"/>'s rules: receipts, dependencies, drift, Hardcore), dependencies before
    /// their dependents when enabling and after them when disabling; a mod that path refuses (or whose receipt cannot be read) stays as it
    /// was and is reported, the others still change. Then every hand-copied folder's <c>loadorder.txt</c> line gets or loses its <c>#</c>
    /// (<see cref="ExtraOptionsMods.SetAllEnabled"/>; enabling also clears a <c>#</c> left on an enabled managed mod's line); then, whatever
    /// stopped the operation, the Wine overrides and OD-232's load-first mods (<paramref name="loadFirst"/>, the Commons) back on top.
    /// Retired installations are only ever disabled. Review 3.1.2: only what actually changed leaves the Hardcore holds
    /// (<see cref="HardcoreHeldFile"/>, <see cref="HandHeldFile"/>), under the same lock; a refused mod stays held.
    /// </summary>
    /// <param name="loadFirst">The folders kept first in <c>loadorder.txt</c> (installed load-first mods and the catalog's, by id or folder).</param>
    public ModToggleAllResult SetEnabledAll(string game, bool enabled, IEnumerable<string>? loadFirst = null, IProgress<string>? progress = null, bool hardcore = false)
    {
        ArgumentNullException.ThrowIfNull(game);
        using var gate = Lock();
        var snapshots = InstalledSnapshotsNow(out var unreadable);
        _bulk = new(snapshots, unreadable, new HashSet<string>(StringComparer.Ordinal));
        var changed = new List<string>(); var failed = new List<ModToggleFailure>(); var manualFolders = new List<string>();
        var first = (loadFirst ?? []).Concat(snapshots.Where(m => m.LoadFirst && !m.Retired && m.ModsGame == game).Select(ExtraOptionsMods.FolderOf).OfType<string>()).ToArray();
        try
        {
            var mine = snapshots.Where(m => m.ModsGame == game && (!enabled || !m.Retired)).ToList();
            var ordered = DependenciesFirst(mine);
            if (!enabled) ordered.Reverse();
            foreach (var manifest in ordered)
            {
                try
                {
                    if (!SetEnabledLocked(manifest, enabled, progress, hardcore)) continue;
                    changed.Add(manifest.Id);
                    if (enabled) _bulk.Verified.Add(manifest.Digest); // its files were just written as the receipt names them
                }
                catch (Exception error) when (IsStateError(error) && !Stops(error)) { failed.Add(new(manifest.Id, Code(error))); }
            }
            // hand-copied folders; a managed mod's folder is its receipt's (disabled: its files are gone), except a '#' left on an enabled one
            var managedOff = snapshots.Where(m => m.ModsGame == game && ReceiptOrNull(m, null) is not { Enabled: true }).Select(ExtraOptionsMods.FolderOf).OfType<string>();
            var managed = snapshots.Where(m => m.ModsGame == game).Select(ExtraOptionsMods.FolderOf).OfType<string>();
            var manual = ExtraOptionsMods.SetAllEnabled(_azimuth, enabled, (enabled ? managedOff : managed).ToHashSet(StringComparer.OrdinalIgnoreCase), game, manualFolders);
            return new(changed.AsReadOnly(), failed.AsReadOnly(), manual) { ManualFolders = manualFolders.AsReadOnly() };
        }
        finally
        {
            _bulk = null;
            FinishBulk(null, first.Length > 0 ? first : null, game);
            // what changed is the player's choice now (a crash or a stop above keeps what did not)
            try
            {
                if (HardcoreHeldFile?.Invoke(game) is { } managedHeld && changed.Count > 0) ForgetHeldIds(managedHeld, changed);
                if (HandHeldFile?.Invoke(game) is { } handHeld)
                {
                    var folders = manualFolders.Concat(snapshots.Where(m => changed.Contains(m.Id, StringComparer.Ordinal)).Select(ExtraOptionsMods.FolderOf).OfType<string>()).ToList();
                    if (folders.Count > 0) ExtraOptionsMods.ForgetHeld(handHeld, folders);
                }
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException) { }
        }
    }

    /// <summary>
    /// Alex 2026-11-10 ("PLAY HARDCORE must never refuse because of mods"): before a launch, game closed, in one operation under one lock.
    /// <paramref name="hardcore"/>: every enabled managed mod of <paramref name="game"/> a Hardcore profile does not keep
    /// (<see cref="AllowedInHardcore"/>, OD-236: only the mods that do not count as modded stay), and every enabled mod that depends on one
    /// of them, is turned off through its receipt; their ids are added to <paramref name="heldFile"/> first, so a crash in between still
    /// gives them back. Otherwise (a Noob or offline launch): the held mods still installed and off are turned back on (dependencies first)
    /// and the file is emptied; one that cannot come back now (a missing dependency, drift) stays held for the next launch. Nothing is ever
    /// refused: a mod whose receipt cannot be read is skipped with a note (review 3.1.2), and the follow-up steps always run.
    /// </summary>
    public ModHoldResult HoldForHardcore(string game, bool hardcore, string heldFile, IProgress<string>? progress = null)
    {
        ArgumentNullException.ThrowIfNull(game); ArgumentNullException.ThrowIfNull(heldFile);
        var held = ReadHeldIds(heldFile);
        if (!hardcore && held.Count == 0) return ModHoldResult.None;
        if (!Directory.Exists(BackupRoot) || !Directory.EnumerateFiles(BackupRoot, "*.receipt.json").Any())
        {
            if (held.Count > 0) WriteHeldIds(heldFile, []);
            return ModHoldResult.None;
        }
        var notes = new List<string>(); var changed = new List<string>();
        using var gate = Lock();
        var snapshots = InstalledSnapshotsNow(out var unreadable);
        _bulk = new(snapshots, unreadable, new HashSet<string>(StringComparer.Ordinal));
        var touched = false;
        try
        {
            var mine = snapshots.Where(m => m.ModsGame == game).ToList();
            var on = mine.ToDictionary(m => m.Id, m => ReceiptOrNull(m, notes)?.Enabled, StringComparer.Ordinal);
            bool On(ModManifest m) => on.GetValueOrDefault(m.Id) == true;
            if (hardcore)
            {
                var off = mine.Where(m => On(m) && !AllowedInHardcore(m)).Select(m => m.Id).ToHashSet(StringComparer.Ordinal);
                // an enabled mod that needs one of them goes too (it could not stay on without it)
                for (var grew = true; grew;)
                {
                    grew = false;
                    foreach (var m in mine.Where(m => !off.Contains(m.Id) && On(m) && m.Dependencies.Any(d => off.Contains(d.Id))))
                        grew |= off.Add(m.Id);
                }
                if (off.Count == 0) return new([], notes.AsReadOnly());
                WriteHeldIds(heldFile, [.. held.Union(off, StringComparer.Ordinal)]);
                touched = true;
                var ordered = DependenciesFirst(mine.Where(m => off.Contains(m.Id)).ToList());
                ordered.Reverse(); // dependents first
                foreach (var manifest in ordered)
                {
                    try { if (SetEnabledLocked(manifest, false, progress, hardcore: false)) { changed.Add(manifest.Id); notes.Add("mods: " + manifest.Id + " off for Hardcore"); } }
                    catch (Exception error) when (IsStateError(error) && !Stops(error))
                    { notes.Add("mods: " + manifest.Id + " not turned off for Hardcore (" + Code(error) + ")"); }
                }
            }
            else
            {
                var keep = new List<string>();
                touched = true;
                foreach (var manifest in DependenciesFirst(mine.Where(m => held.Contains(m.Id, StringComparer.Ordinal)).ToList()))
                {
                    if (on.GetValueOrDefault(manifest.Id) is not false) { if (on.GetValueOrDefault(manifest.Id) is null) keep.Add(manifest.Id); continue; } // on already, or unreadable (kept)
                    try
                    {
                        if (manifest.Retired) continue; // never enabled again (OD-232)
                        if (SetEnabledLocked(manifest, true, progress, hardcore: false)) { changed.Add(manifest.Id); notes.Add("mods: " + manifest.Id + " on again after Hardcore"); }
                        _bulk.Verified.Add(manifest.Digest);
                    }
                    catch (Exception error) when (IsStateError(error) && !Stops(error))
                    { keep.Add(manifest.Id); notes.Add("mods: " + manifest.Id + " stays off for now (" + Code(error) + ")"); }
                }
                // the file is this game's: what came back (or is gone) leaves it, what could not stays to retry
                WriteHeldIds(heldFile, keep);
            }
            return new(changed.AsReadOnly(), notes.AsReadOnly());
        }
        finally
        {
            _bulk = null;
            if (touched) FinishBulk(notes);
        }
    }

    /// <summary>
    /// Alex 2026-11-10 (review 3.1.2, finding 6): an enabled mod in a retired format (OD-232, <see cref="ModLaunchPreparation.BlocksOnline"/>)
    /// no longer stops an online launch: it is turned off through its receipt before the session, in one operation under one lock, and not
    /// given back (OD-232 refuses enabling a retired format again; it stays off until the player removes it). Returns the ids turned off;
    /// one that cannot be turned off is skipped with a note.
    /// </summary>
    public ModHoldResult DisableRetiredForOnline(string game, IProgress<string>? progress = null)
    {
        ArgumentNullException.ThrowIfNull(game);
        if (!Directory.Exists(BackupRoot) || !Directory.EnumerateFiles(BackupRoot, "*.receipt.json").Any()) return ModHoldResult.None;
        var notes = new List<string>(); var changed = new List<string>();
        using var gate = Lock();
        var snapshots = InstalledSnapshotsNow(out var unreadable);
        var retired = snapshots.Where(m => m.ModsGame == game && ModLaunchPreparation.BlocksOnline(m) && ReceiptOrNull(m, notes) is { Enabled: true }).ToList();
        if (retired.Count == 0) return new([], notes.AsReadOnly());
        _bulk = new(snapshots, unreadable, new HashSet<string>(StringComparer.Ordinal));
        try
        {
            var ordered = DependenciesFirst(retired);
            ordered.Reverse(); // dependents first
            foreach (var manifest in ordered)
            {
                try
                {
                    // an enabled mod that needs it would refuse the change (CheckDependents): it is a retired-format mod too or it was
                    // listed already; otherwise the note says so and the launch goes on (the gate still decides)
                    if (SetEnabledLocked(manifest, false, progress, hardcore: false)) { changed.Add(manifest.Id); notes.Add("mods: " + manifest.Id + " off (retired format, online)"); }
                    ForgetHeldLocked(manifest); // never given back by a hold either
                }
                catch (Exception error) when (IsStateError(error) && !Stops(error))
                { notes.Add("mods: " + manifest.Id + " (retired format) not turned off (" + Code(error) + ")"); }
            }
            return new(changed.AsReadOnly(), notes.AsReadOnly());
        }
        finally { _bulk = null; FinishBulk(notes); }
    }

    /// <summary>Removes <paramref name="ids"/> from a managed hold file (the rest stays).</summary>
    public static void ForgetHeldIds(string heldFile, IEnumerable<string> ids)
    {
        ArgumentNullException.ThrowIfNull(heldFile); ArgumentNullException.ThrowIfNull(ids);
        var held = ReadHeldIds(heldFile);
        var gone = ids.ToHashSet(StringComparer.Ordinal);
        if (held.RemoveAll(gone.Contains) > 0) WriteHeldIds(heldFile, held);
    }

    /// <summary>Enable all / Disable all is the player's choice: what a Hardcore launch held is not given back later.</summary>
    public static void ForgetHardcoreHeld(string heldFile)
    {
        ArgumentNullException.ThrowIfNull(heldFile);
        if (ReadHeldIds(heldFile).Count > 0) WriteHeldIds(heldFile, []);
    }

    private static List<string> ReadHeldIds(string heldFile)
    {
        try { return File.Exists(heldFile) ? System.Text.Json.JsonSerializer.Deserialize<List<string>>(File.ReadAllBytes(heldFile)) ?? [] : []; }
        catch (System.Text.Json.JsonException) { return []; }
    }

    /// <summary>Written whole, then moved over the old one (emptied rather than deleted, as the hand-copied mods' file).</summary>
    private static void WriteHeldIds(string heldFile, List<string> ids)
    {
        if (ids.Count == 0 && !File.Exists(heldFile)) return;
        Directory.CreateDirectory(Path.GetDirectoryName(heldFile)!);
        var temporary = heldFile + ".fi-new";
        File.WriteAllBytes(temporary, System.Text.Json.JsonSerializer.SerializeToUtf8Bytes(ids));
        File.Move(temporary, heldFile, overwrite: true);
    }

    /// <summary>The mods with each one's dependencies (among them) before it; a cycle keeps the order it was found in.</summary>
    private static List<ModManifest> DependenciesFirst(IReadOnlyList<ModManifest> mods)
    {
        var byId = mods.GroupBy(m => m.Id, StringComparer.Ordinal).ToDictionary(g => g.Key, g => g.First(), StringComparer.Ordinal);
        var ordered = new List<ModManifest>(); var done = new HashSet<string>(StringComparer.Ordinal); var visiting = new HashSet<string>(StringComparer.Ordinal);
        void Visit(ModManifest mod)
        {
            if (done.Contains(mod.Id) || !visiting.Add(mod.Id)) return;
            foreach (var dependency in mod.Dependencies)
                if (byId.TryGetValue(dependency.Id, out var needed)) Visit(needed);
            visiting.Remove(mod.Id);
            if (done.Add(mod.Id)) ordered.Add(mod);
        }
        foreach (var mod in mods.OrderBy(m => m.Id, StringComparer.Ordinal)) Visit(mod);
        return ordered;
    }
}
