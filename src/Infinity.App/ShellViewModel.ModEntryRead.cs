// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10 (3.1.1 build #3): "Every action I take, install, enable, disable, makes a new read of all mods from the folder […]
/// The 'reading your mods' should actually happen when the launcher starts, and before playing." Every mod is read (all receipts, drift,
/// the catalog's facts) only at the start (in the background), before Play (the launch preparation) and on ↻ (and by the bulk changes:
/// a set, Recover, Resolve, Relink, the layout move, the catalog, a game switch). After one mod's install, enable, disable, update,
/// uninstall, repair or settings save, only that installation is read again, with the page's kept hashes, and only the cards its
/// dependencies and conflicts tie to it are decided again; no "Reading your mods…" toast.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The running full read (<see cref="ModReadLoopAsync"/>), awaited by a one-mod read so it lands on top of it.</summary>
    private Task? _modReadLoop;

    /// <summary>The catalog the cards' facts were built from (a one-mod read on another catalog reads every mod instead).</summary>
    private IReadOnlyList<ModManifest>? _modFactsCatalog;

    /// <summary>Test seam: every one-mod read: the ids asked, the installations inspected, the files hashed and its duration.</summary>
    internal List<(int Asked, int Decided, long Inspected, long Hashed, TimeSpan Took)> ModEntryReadsForTests { get; } = [];

    /// <summary>A page no read has filled yet asks for one; otherwise the running read (if any) is awaited and nothing is read again.</summary>
    internal Task EnsureModsReadAsync()
    {
        // the built-in card (and whatever is already known) at once, as a read's start shows it: never an empty page
        if (ModCards.Count == 0) { try { RefreshModCards(); } catch (Exception error) when (error is not OutOfMemoryException) { Log("mods: cards not built: " + ErrorChain(error)); } }
        if (_modReadRunning && _modReadLoop is { } running) return running;
        return ModReadsApplied == 0 ? RefreshModsAsync() : Task.CompletedTask;
    }

    /// <summary>
    /// After an operation on <paramref name="changed"/>: those installations are read again (<see cref="ManagedModFiles.InspectSome"/>, the
    /// display manager and kept hashes), the facts of the cards tied to them are decided again, and the page is rebuilt from memory. A full
    /// read that runs (or none applied yet, or another catalog since) is waited for or done instead, so the page is never older than the change.
    /// </summary>
    internal async Task RefreshModEntriesAsync(IReadOnlyCollection<string> changed)
    {
        ArgumentNullException.ThrowIfNull(changed);
        // one at a time: each builds on the state the one before applied (two quick actions never lose the first one's change)
        await _modEntryGate.WaitAsync();
        try { await RefreshModEntriesNowAsync(changed); }
        finally { _modEntryGate.Release(); }
    }

    private readonly SemaphoreSlim _modEntryGate = new(1, 1);

    private async Task RefreshModEntriesNowAsync(IReadOnlyCollection<string> changed)
    {
        // a full read that started before or during the change lands first; this read goes on top of it
        while (_modReadRunning && _modReadLoop is { } running) await running;
        var factory = ModManagerFactory();
        var catalog = _manifestCatalog;
        if (factory is null || ModReadsApplied == 0 || _modStateError is not null || !ReferenceEquals(_modFactsCatalog, catalog)) { await RefreshModsAsync(); return; }
        var previous = _modHealth; var oldFacts = _modFacts; var appliedBefore = ModReadsApplied;
        var decide = TiedIds(changed, catalog, previous);
        var watch = System.Diagnostics.Stopwatch.StartNew();
        var hashedBefore = ManagedModFiles.KeptHashesComputed; var inspectedBefore = ManagedModFiles.InstallationsInspected;
        (ModHealth Health, IReadOnlyList<ModCardFacts> Facts)? read;
        try
        {
            read = await Task.Run<(ModHealth, IReadOnlyList<ModCardFacts>)?>(() =>
            {
                ManagedModFiles manager;
                try { manager = factory(); }
                catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return null; }
                manager.ForDisplay = true; // reads only: each receipt once, the kept hashes, nothing written
                var health = manager.InspectSome(previous, changed);
                var decided = new Dictionary<string, ModCardFacts?>(StringComparer.Ordinal);
                foreach (var id in decide)
                {
                    var installed = health.Installed.FirstOrDefault(i => i.Id == id);
                    decided[id] = catalog.FirstOrDefault(m => m.Id == id) is { } listed ? ListedFact(manager, listed, installed)
                        : installed?.Manifest is not null ? UnlistedFact(manager, installed) : null;
                }
                // in the full read's order: the catalog's, then the installed ones it no longer lists
                var facts = new List<ModCardFacts>();
                var placed = new HashSet<string>(StringComparer.Ordinal);
                foreach (var fact in oldFacts)
                {
                    var id = fact.Manifest.Id;
                    if (!decided.TryGetValue(id, out var next)) facts.Add(fact);
                    else if (next is not null) facts.Add(next);
                    placed.Add(id);
                }
                foreach (var (id, next) in decided) if (next is not null && !placed.Contains(id)) facts.Add(next);
                return (health, facts);
            });
        }
        catch (Exception error) when (error is not OutOfMemoryException) { Log("mods: entry not read: " + ErrorChain(error)); read = null; }
        if (read is not { } state || !ReferenceEquals(catalog, _manifestCatalog)) { await RefreshModsAsync(); return; }
        ModEntryReadsForTests.Add((changed.Count, decide.Count, ManagedModFiles.InstallationsInspected - inspectedBefore, ManagedModFiles.KeptHashesComputed - hashedBefore, watch.Elapsed));
        if (ModEntryReadsForTests.Count > 64) ModEntryReadsForTests.RemoveAt(0);
        if (watch.ElapsedMilliseconds >= 1000) Log("mods: " + string.Join(", ", changed) + " read in " + watch.ElapsedMilliseconds + " ms (" + decide.Count + " cards decided)");
        // a full read that began meanwhile (a catalog, a game switch) is newer than this one: it fills the page (awaited, so the caller
        // sees the state after its change)
        if (_modReadRunning) { if (_modReadLoop is { } newer) await newer; return; }
        if (ModReadsApplied != appliedBefore) return; // one already landed after this read began
        _modHealth = state.Health; _modFacts = state.Facts;
        try
        {
            NoteDuplicateDiscordMod();
            RefreshModCards();
            await RefreshExtraOptionsModsAsync(); // the mods folder's listing (an install adds a folder): no hashing
            if (_selectedMod is { HasOptions: true } && !_iniDirty) await LoadSelectedOptionsAsync();
        }
        finally { Changed(string.Empty); }
    }

    /// <summary>
    /// The mods whose card facts may change with <paramref name="changed"/> (as <see cref="ListedFact"/> decides them): those mods; their
    /// direct dependencies (whose Disable waits for enabled dependents, <see cref="ManagedModFiles.CheckDependents"/>); every mod that depends
    /// on one of them, followed through (<see cref="ManagedModFiles.CheckDependencies"/> is recursive); and the mods a conflict ties to them,
    /// either way. The catalog's and the installed manifests, an update's next one included.
    /// </summary>
    internal static HashSet<string> TiedIds(IEnumerable<string> changed, IReadOnlyList<ModManifest> catalog, ModHealth health)
    {
        var manifests = catalog.Concat(health.Installed.Select(i => i.Manifest).OfType<ModManifest>()).ToArray();
        var result = new HashSet<string>(changed, StringComparer.Ordinal);
        var direct = new HashSet<string>(StringComparer.Ordinal);
        foreach (var manifest in manifests)
        {
            var conflicts = manifest.Document.TryGetProperty("conflicts", out var list) && list.ValueKind == System.Text.Json.JsonValueKind.Array
                ? list.EnumerateArray().Select(c => c.GetString()).OfType<string>().ToArray() : [];
            if (result.Contains(manifest.Id)) { direct.UnionWith(manifest.Dependencies.Select(d => d.Id)); direct.UnionWith(conflicts); }
            else if (conflicts.Any(result.Contains)) direct.Add(manifest.Id);
        }
        // the dependents, through every level (a mod that depends on a dependent of a changed one is decided again too)
        var queue = new Queue<string>(result);
        while (queue.TryDequeue(out var id))
            foreach (var dependent in manifests.Where(m => m.Dependencies.Any(d => d.Id == id)).Select(m => m.Id))
                if (result.Add(dependent)) queue.Enqueue(dependent);
        result.UnionWith(direct);
        result.Remove(ManagedModFiles.BuiltInExtraOptions);
        return result;
    }
}
