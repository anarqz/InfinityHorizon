// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Mods;

/// <summary>A mod <see cref="ManagedModFiles.SetEnabledAll"/> left as it was, and why (a <see cref="ModErrors"/> code).</summary>
public sealed record ModToggleFailure(string Id, string Code);

/// <summary>
/// What <see cref="ManagedModFiles.SetEnabledAll"/> did: the managed mods it switched, those it could not (each with its reason), and how
/// many lines of hand-copied folders it switched in <c>loadorder.txt</c>.
/// </summary>
public sealed record ModToggleAllResult(IReadOnlyList<string> Changed, IReadOnlyList<ModToggleFailure> Failed, int ManualChanged);

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

    /// <summary>A dependency's files are hashed once per <see cref="SetEnabledAll"/> (a Common under 43 packs), every time otherwise.</summary>
    private bool VerifiedOnce(ModManifest manifest)
    {
        if (_bulk is { } bulk && bulk.Verified.Contains(manifest.Digest)) return true;
        var verified = Verified(manifest);
        if (verified) _bulk?.Verified.Add(manifest.Digest);
        return verified;
    }

    /// <summary>
    /// Alex 2026-11-10: "Enable all" / "Disable all" for one game's mods, in one operation under one lock. Every managed mod of the game
    /// goes through the single-mod path (<see cref="SetEnabled"/>'s rules: receipts, dependencies, drift, Hardcore), dependencies before
    /// their dependents when enabling and after them when disabling; a mod that path refuses stays as it was and is reported, the others
    /// still change. Then every hand-copied folder's <c>loadorder.txt</c> line gets or loses its <c>#</c>
    /// (<see cref="ExtraOptionsMods.SetAllEnabled"/>; enabling also clears a <c>#</c> left on an enabled managed mod's line); then the Wine
    /// overrides, and OD-232's load-first mods (<paramref name="loadFirst"/>, the Commons) back on top. Retired installations are only ever
    /// disabled.
    /// </summary>
    /// <param name="loadFirst">The folders kept first in <c>loadorder.txt</c> (installed load-first mods and the catalog's, by id or folder).</param>
    public ModToggleAllResult SetEnabledAll(string game, bool enabled, IEnumerable<string>? loadFirst = null, IProgress<string>? progress = null, bool hardcore = false)
    {
        ArgumentNullException.ThrowIfNull(game);
        using var gate = Lock();
        var snapshots = InstalledSnapshotsNow(out var unreadable);
        _bulk = new(snapshots, unreadable, new HashSet<string>(StringComparer.Ordinal));
        var changed = new List<string>(); var failed = new List<ModToggleFailure>();
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
                catch (ModException error) when (error.Code is not (ModErrors.Recovery or ModErrors.Busy or ModErrors.GameRunning))
                { failed.Add(new(manifest.Id, error.Code)); }
            }
            // hand-copied folders; a managed mod's folder is its receipt's (disabled: its files are gone), except a '#' left on an enabled one
            var managedOff = snapshots.Where(m => m.ModsGame == game && Receipt(m) is not { Enabled: true }).Select(ExtraOptionsMods.FolderOf).OfType<string>();
            var managed = snapshots.Where(m => m.ModsGame == game).Select(ExtraOptionsMods.FolderOf).OfType<string>();
            var manual = ExtraOptionsMods.SetAllEnabled(_azimuth, enabled, (enabled ? managedOff : managed).ToHashSet(StringComparer.OrdinalIgnoreCase), game);
            ReconcileWineOverrides();
            var first = (loadFirst ?? []).Concat(snapshots.Where(m => m.LoadFirst && !m.Retired && m.ModsGame == game).Select(ExtraOptionsMods.FolderOf).OfType<string>()).ToArray();
            if (first.Length > 0) ExtraOptionsMods.ArrangeLoadOrder(_azimuth, first, game);
            return new(changed.AsReadOnly(), failed.AsReadOnly(), manual);
        }
        finally { _bulk = null; }
    }

    /// <summary>
    /// Alex 2026-11-10 ("PLAY HARDCORE must never refuse because of mods"): before a launch, game closed, in one operation under one lock.
    /// <paramref name="hardcore"/>: every enabled managed mod of <paramref name="game"/> a Hardcore profile does not keep
    /// (<see cref="AllowedInHardcore"/>: the official car packs and the mods for both modes stay), and every enabled mod that depends on one
    /// of them, is turned off through its receipt; their ids are added to <paramref name="heldFile"/> first, so a crash in between still
    /// gives them back. Otherwise (a Noob or offline launch): the held mods still installed and off are turned back on (dependencies first)
    /// and the file is emptied; one that cannot come back now (a missing dependency, drift) stays held for the next launch. Nothing is ever
    /// refused.
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
        try
        {
            var mine = snapshots.Where(m => m.ModsGame == game).ToList();
            bool On(ModManifest m) => Receipt(m) is { Enabled: true };
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
                if (off.Count == 0) return ModHoldResult.None;
                WriteHeldIds(heldFile, [.. held.Union(off, StringComparer.Ordinal)]);
                var ordered = DependenciesFirst(mine.Where(m => off.Contains(m.Id)).ToList());
                ordered.Reverse(); // dependents first
                foreach (var manifest in ordered)
                {
                    try { if (SetEnabledLocked(manifest, false, progress, hardcore: false)) { changed.Add(manifest.Id); notes.Add("mods: " + manifest.Id + " off for Hardcore"); } }
                    catch (ModException error) when (error.Code is not (ModErrors.Recovery or ModErrors.Busy or ModErrors.GameRunning))
                    { notes.Add("mods: " + manifest.Id + " not turned off for Hardcore (" + error.Code + ")"); }
                }
            }
            else
            {
                var keep = new List<string>();
                foreach (var manifest in DependenciesFirst(mine.Where(m => held.Contains(m.Id, StringComparer.Ordinal)).ToList()))
                {
                    if (On(manifest)) continue; // the player turned it on already
                    try
                    {
                        if (manifest.Retired) continue; // never enabled again (OD-232)
                        if (SetEnabledLocked(manifest, true, progress, hardcore: false)) { changed.Add(manifest.Id); notes.Add("mods: " + manifest.Id + " on again after Hardcore"); }
                        _bulk.Verified.Add(manifest.Digest);
                    }
                    catch (ModException error) when (error.Code is not (ModErrors.Recovery or ModErrors.Busy or ModErrors.GameRunning))
                    { keep.Add(manifest.Id); notes.Add("mods: " + manifest.Id + " stays off for now (" + error.Code + ")"); }
                }
                // the file is this game's: what came back (or is gone) leaves it, what could not stays to retry
                WriteHeldIds(heldFile, keep);
            }
            ReconcileWineOverrides();
            KeepLoadFirstOnTop();
            return new(changed.AsReadOnly(), notes.AsReadOnly());
        }
        finally { _bulk = null; }
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
