// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using Infinity.Core.Matchmaking;
using Infinity.Core.Mods;
using Infinity.Core.Process;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>One folder of <c>&lt;Azimuth&gt;\mods</c> on the Mods page (OD-126): what it holds and what the last game run said.</summary>
public sealed record ExtraOptionsModItem(string Folder, string Name, string Details, string Status, bool Attention, bool Enabled, bool CanToggle)
{
    /// <summary>The game whose mods folder holds it (OD-233): a toggle or delete acts there, whatever game is chosen meanwhile.</summary>
    public string Game { get; init; } = ModManifest.DefaultGame;
    public string Toggle { get; init; } = "";
    /// <summary>Owner 2026-10-28: a manual mod, once off, can be deleted from the mods folder (asked twice).</summary>
    public bool CanDelete { get; init; }
    public string DeleteLabel { get; init; } = "";
}

public sealed partial class ShellViewModel
{
    /// <summary>Remembers the mods a Hardcore launch switched off, to switch them back on at the next other launch (one file per game).</summary>
    private const string HardcoreHeldFile = "extraoptions-hardcore-held.json";

    private static string HardcoreHeldFileFor(string game) => game == ModManifest.DefaultGame ? HardcoreHeldFile : "extraoptions-hardcore-held-" + game + ".json";

    public ObservableCollection<ExtraOptionsModItem> ExtraOptionsModItems { get; } = [];
    public bool HasExtraOptionsMods => ExtraOptionsModItems.Count > 0;
    public string ExtraOptionsModsFolder => _install is null || !ExtraOptionsLayout.HasModsFolder(CurrentGame.Id) ? ""
        : S.Format("eomods.folder", ("path", ExtraOptionsMods.ModsDirectory(_install.Root, CurrentGame.Id)));
    public string ExtraOptionsModsHint => S[ProfileHardcore ? "eomods.hint_hardcore" : "eomods.hint"];

    /// <summary>
    /// Reads the chosen game's mods folder (<c>mods\&lt;game folder&gt;</c>, OD-233), its <c>loadorder.txt</c> and the last
    /// <c>FH2_ExtraOptions.log</c> / <c>FM5_ExtraOptions.log</c> off the UI thread. Owner 2026-11-10: FM5 lists its hand-copied mods as FH2 does.
    /// </summary>
    private async Task RefreshExtraOptionsModsAsync()
    {
        var game = CurrentGame.Id;
        if (_install is not { } install || !ExtraOptionsLayout.HasModsFolder(game)) { ExtraOptionsModItems.Clear(); Changed(nameof(HasExtraOptionsMods)); return; }
        IReadOnlyList<ExtraOptionsMod> mods; IReadOnlyDictionary<string, ExtraOptionsModStatus> status;
        try { (mods, status) = await Task.Run(() => { var m = ExtraOptionsMods.Scan(install.Root, game); return (m, ExtraOptionsMods.ReadStatus(install.Root, m, game)); }); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException)
        { Log("extraoptions mods: not read (" + e.GetType().Name + ")"); mods = []; status = new Dictionary<string, ExtraOptionsModStatus>(); }
        if (CurrentGame.Id != game) return; // another game was chosen meanwhile: its own refresh fills the list
        ExtraOptionsModItems.Clear();
        // Owner 2026-10-28: the launcher's own mods have their cards; this list is what was put there by hand (or added from a ZIP)
        var managed = ManagedExtraOptionsFolders(game);
        // a folder ExtraOptions loads nothing from (what an uninstalled mod's own settings leave behind) is no mod: not listed
        foreach (var mod in mods.Where(m => !managed.Contains(m.Folder) && (m.HasMedia || m.HasDatabase || m.HasStrings || m.Plugins > 0 || m.ManifestError is not null)))
            ExtraOptionsModItems.Add(Item(mod, mods, status) with { Game = game });
        Changed(nameof(HasExtraOptionsMods)); Changed(nameof(ExtraOptionsModsFolder)); Changed(nameof(ExtraOptionsModsHint));
    }

    /// <summary>
    /// Owner 2026-11-10: the FM5 folders OD-233's catalog-only rule held off (<see cref="ExtraOptionsMods.Fm5CatalogOnlyHeldFile"/>) come
    /// back on, once, game closed; then the hold is gone. Every server state the launcher keeps is read (the rule wrote the active one).
    /// </summary>
    /// <remarks>No view-model state: runs on any thread; returns the log lines.</remarks>
    private static List<string> RestoreFm5CatalogOnlyHold(string azimuthRoot, IEnumerable<string> stateDirectories)
    {
        var notes = new List<string>();
        foreach (var held in stateDirectories.Distinct(StringComparer.OrdinalIgnoreCase).Select(d => Path.Combine(d, ExtraOptionsMods.Fm5CatalogOnlyHeldFile)))
        {
            if (!File.Exists(held)) continue;
            try { notes.AddRange(ExtraOptionsMods.RestoreCatalogOnlyHold(azimuthRoot, held, "fm5")); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException or ArgumentException)
            { notes.Add("extraoptions mods: FM5 held folders not restored yet (" + e.GetType().Name + "); retried at the next start or launch"); }
        }
        return notes;
    }

    /// <summary>The restore above, off the UI thread and only with the game closed (ExtraOptions rewrites <c>loadorder.txt</c> at start).</summary>
    private async Task RestoreFm5CatalogOnlyHoldAsync()
    {
        if (_install is not { } install) return;
        string[] states = [_stateDirectory, ModHome];
        foreach (var note in await Task.Run(() => GameRunning() ? [] : RestoreFm5CatalogOnlyHold(install.Root, states))) Log(note);
    }

    private ExtraOptionsModItem Item(ExtraOptionsMod mod, IReadOnlyList<ExtraOptionsMod> all, IReadOnlyDictionary<string, ExtraOptionsModStatus> status)
    {
        var parts = new List<string>();
        if (mod.HasMedia) parts.Add(S["eomods.part_media"]);
        if (mod.HasDatabase) parts.Add(S["eomods.part_database"]);
        if (mod.HasStrings) parts.Add(S["eomods.part_strings"]);
        if (mod.Plugins > 0) parts.Add(S.Format("eomods.part_plugins", ("count", mod.Plugins.ToString(System.Globalization.CultureInfo.InvariantCulture))));
        if (parts.Count == 0) parts.Add(S["eomods.part_empty"]);
        var details = (mod.Version is { Length: > 0 } v ? "v" + v + " · " : "") + string.Join(" · ", parts)
            + (mod.Folder != mod.Name ? " · " + mod.Folder : "");
        // ExtraOptions' own rules first (it will refuse these), then the last run's verdict. The order is the launcher's to fix
        // (every launch puts a mod after its dependencies), so only a missing or disabled dependency is the player's.
        var missing = mod.Dependencies.Where(d => !all.Any(o => o.Enabled && o.ManifestError is null
            && (o.Id.Equals(d, StringComparison.OrdinalIgnoreCase) || o.Folder.Equals(d, StringComparison.OrdinalIgnoreCase)))).ToList();
        string text; var attention = false;
        if (!mod.Enabled) text = S["eomods.off"];
        else if (mod.ManifestError is not null) { text = S["eomods.bad_manifest"]; attention = true; }
        else if (missing.Count > 0) { text = S.Format("eomods.dependency", ("mods", string.Join(", ", missing))); attention = true; }
        else if (status.TryGetValue(mod.Folder, out var seen))
        {
            text = seen.State switch
            {
                ExtraOptionsModState.Loaded => S["eomods.loaded"],
                ExtraOptionsModState.Pending => S["eomods.pending"],
                ExtraOptionsModState.Failed => S.Format("eomods.failed", ("reason", seen.Messages.LastOrDefault(m => !m.StartsWith("addon rejected", StringComparison.Ordinal)) ?? seen.Messages[^1])),
                _ => S["eomods.not_seen"],
            };
            attention = seen.State == ExtraOptionsModState.Failed;
        }
        else text = S["eomods.not_seen"];
        return new(mod.Folder, mod.Name, details, text, attention, mod.Enabled, ModsIdle)
        {
            Toggle = S[mod.Enabled ? "mods.disable_action" : "mods.enable_action"],
            CanDelete = !mod.Enabled && ModsIdle,
            DeleteLabel = S[string.Equals(_deletingManualMod, mod.Folder, StringComparison.OrdinalIgnoreCase) ? "eomods.delete_confirm" : "eomods.delete"],
        };
    }

    /// <summary>The manual mod whose Delete was pressed once; the second press deletes it.</summary>
    private string? _deletingManualMod;

    /// <summary>Owner 2026-10-28: deletes an off manual mod's folder from <c>mods\</c> (and its load-order line), game closed, after a second press.</summary>
    public async Task DeleteExtraOptionsModAsync(ExtraOptionsModItem item)
    {
        if (_install is not { } install || !ModsIdle || item.Enabled) return;
        if (!string.Equals(_deletingManualMod, item.Folder, StringComparison.OrdinalIgnoreCase))
        {
            _deletingManualMod = item.Folder;
            await RefreshExtraOptionsModsAsync();
            return;
        }
        _deletingManualMod = null;
        if (GameRunning()) { Message = S["mods.close_game"]; await RefreshExtraOptionsModsAsync(); return; }
        try
        {
            await Task.Run(() => ExtraOptionsMods.DeleteFolder(install.Root, item.Folder, item.Game));
            Log("extraoptions mods: " + item.Folder + " deleted");
            Message = S["eomods.deleted"];
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException or ArgumentException)
        { Log("extraoptions mods: " + item.Folder + " not deleted (" + e.GetType().Name + ")"); Message = S["eomods.not_deleted"]; }
        await RefreshExtraOptionsModsAsync();
    }

    /// <summary>Switches one mod in <c>loadorder.txt</c>; only with the game closed (ExtraOptions rewrites the file at start).</summary>
    public async Task ToggleExtraOptionsModAsync(ExtraOptionsModItem item)
    {
        if (_install is not { } install || !ModsIdle) return;
        if (GameRunning()) { Message = S["mods.close_game"]; return; }
        try
        {
            await Task.Run(() => ExtraOptionsMods.SetEnabled(install.Root, item.Folder, !item.Enabled, item.Game));
            Log("extraoptions mods: " + item.Folder + (item.Enabled ? " off" : " on"));
            Message = S[item.Enabled ? "eomods.now_off" : "eomods.now_on"];
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException or ArgumentException)
        { Log("extraoptions mods: " + item.Folder + " not changed (" + e.GetType().Name + ")"); Message = S["eomods.not_changed"]; }
        await RefreshExtraOptionsModsAsync();
    }

    /// <summary>
    /// OD-232: the folders kept first in <c>loadorder.txt</c>: installed mods with <see cref="ModManifest.LoadFirst"/>, and the catalog's
    /// load-first ids (a Common copied in by hand is still its id's folder).
    /// </summary>
    private string[] LoadFirstFolders(string game) =>
        _modHealth.Installed.Select(i => i.Manifest).OfType<ModManifest>().Where(m => m.LoadFirst && !m.Retired && m.ModsGame == game).Select(ExtraOptionsMods.FolderOf).OfType<string>()
            .Concat(_manifestCatalog.Where(m => m.LoadFirst && m.ModsGame == game).Select(m => m.Id)).Distinct(StringComparer.OrdinalIgnoreCase).ToArray();

    /// <summary>The folders of the game's mods folder that hold mods the launcher installed (on or off).</summary>
    private HashSet<string> ManagedExtraOptionsFolders(string game = ModManifest.DefaultGame) =>
        _modHealth.Installed.Select(i => i.Manifest).OfType<ModManifest>().Where(m => m.ModsGame == game)
            .Select(ExtraOptionsMods.FolderOf).OfType<string>().ToHashSet(StringComparer.OrdinalIgnoreCase);

    /// <summary>
    /// The folders of the game's mods folder that Hardcore keeps: installed, enabled catalog mods that change nothing for other players
    /// (OD-233: a mod is either both modes, its manifest cosmetic, or Noob only). A folder copied by hand, or a catalog mod that affects
    /// multiplayer, plays only in Noob.
    /// </summary>
    private HashSet<string> HardcoreSafeFolders(string game = ModManifest.DefaultGame) =>
        _modFacts.Select(f => f.Installed is { Enabled: true, Problem: null } installed ? installed.Manifest ?? f.Manifest : null)
            .Where(m => m is not null && m.ModsGame == game && !ModMultiplayer.AffectsMultiplayer(m))
            .Select(m => ExtraOptionsMods.FolderOf(m!)).OfType<string>().ToHashSet(StringComparer.OrdinalIgnoreCase);

    /// <summary>
    /// Owner 2026-10-28: the session's lobby set from what ExtraOptions loads (<see cref="ExtraOptionsModSetSource"/>), planned off the UI
    /// thread after the load order was prepared; catalog mods whose manifest changes nothing for other players stay out. Null when the
    /// mods could not be read while some are enabled (the session would otherwise be admitted as vanilla).
    /// </summary>
    private async Task<IModSetSource?> ExtraOptionsModSetAsync(AzimuthInstall install, DateTime launchUtc)
    {
        // OD-233: each game's ExtraOptions loads its own mods folder (FM5's loader is new in 0.6.0)
        var game = CurrentGame.Id;
        if (!ExtraOptionsLayout.HasModsFolder(game)) return VanillaModSet.Instance;
        // FM5: the state read for this launch (PrepareExtraOptionsMods), as fresh as the folders it just arranged
        var cosmetic = game == "fm5" && _fm5LaunchState is { } fm5 ? fm5.BothModes : HardcoreSafeFolders(game);
        _modSetTooMany = 0;
        try
        {
            var plan = await Task.Run(() => ExtraOptionsModSetSource.PlanFor(install.Root, cosmetic, game));
            Log("mods: lobby set planned from ExtraOptions: " + (plan.Count == 0 ? "none" : string.Join(", ", plan.Select(p => p.Entry.Id + " " + p.Entry.Version))));
            return new ExtraOptionsModSetSource(install.Root, plan, launchUtc, game);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException or ModSetException)
        {
            Log("mods: lobby set not planned (" + e.GetType().Name + (e is ModSetException { TooMany: > 0 } large ? ": " + large.TooMany + " multiplayer mods" : "") + ")");
            if (e is ModSetException { TooMany: > 0 } tooMany) { _modSetTooMany = tooMany.TooMany; return null; } // all of them enabled: never vanilla
            bool anyEnabled;
            try { anyEnabled = ExtraOptionsMods.Scan(install.Root, game).Any(m => m.Enabled); }
            catch (Exception scan) when (scan is IOException or UnauthorizedAccessException or InvalidDataException) { anyEnabled = true; }
            return anyEnabled ? null : VanillaModSet.Instance;
        }
    }

    /// <summary>How many multiplayer mods the last planned lobby set had when it was refused only for its size (else 0).</summary>
    private int _modSetTooMany;

    /// <summary>
    /// Why Play stops when <see cref="ExtraOptionsModSetAsync"/> returned null: too many multiplayer mods for one lobby set
    /// (<see cref="MatchmakingLimits.ModsMax"/>), else files that could not be read or don't match.
    /// </summary>
    private string ModSetRefusal => _modSetTooMany > 0
        ? S.Format("mods.online_too_many", ("count", _modSetTooMany.ToString(System.Globalization.CultureInfo.CurrentCulture)),
            ("max", MatchmakingLimits.ModsMax.ToString(System.Globalization.CultureInfo.CurrentCulture)))
        : S["mods.online_unverified"];

    /// <summary>OD-233: the launcher's FM5 mods as read for one launch: the enabled catalog folders for both modes (what Hardcore keeps).</summary>
    private sealed record Fm5LaunchState(HashSet<string> BothModes);

    /// <summary>The state <see cref="PrepareExtraOptionsMods"/> read for the current FM5 launch; null when it could not be read.</summary>
    private Fm5LaunchState? _fm5LaunchState;

    /// <summary>
    /// FM5's installed mods read fresh from their receipts (never the page's cached state, which may be stale or not loaded yet); null when
    /// any of it could not be read, so nothing is turned off on a guess.
    /// </summary>
    /// <remarks>No view-model state (its factory is taken on the UI thread): runs on a worker.</remarks>
    private static Fm5LaunchState? ReadFm5LaunchState(Func<ManagedModFiles>? factory, string game)
    {
        if (factory is null) return null;
        ModHealth health;
        try { health = factory().Inspect(); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return null; }
        if (health.RecoveryHere || health.Problems.Count > 0 || health.Installed.Any(i => i.Problem is not null || i.Manifest is null)) return null;
        var mine = health.Installed.Where(i => i.Enabled && i.Manifest!.ModsGame == game && !i.Manifest.Retired).ToList();
        HashSet<string> Folders(IEnumerable<ModInstallation> list) => list.Select(i => ExtraOptionsMods.FolderOf(i.Manifest!)).OfType<string>().ToHashSet(StringComparer.OrdinalIgnoreCase);
        return new(Folders(mine.Where(i => !ModMultiplayer.AffectsMultiplayer(i.Manifest!))));
    }

    /// <summary>
    /// Before a launch, after <see cref="PrepareModsLayoutAsync"/>: the game's <c>loadorder.txt</c> puts every mod after its dependencies (and
    /// OD-232's load-first mods on top); Hardcore plays only with <see cref="HardcoreSafeFolders"/>, and any other launch gets back what it
    /// held. Owner 2026-11-10: FM5 follows the same rule (hand-copied mods play in Noob, Hardcore turns them off), its Hardcore set read
    /// fresh from the receipts; the folders OD-233's former catalog-only rule held off come back first (<see cref="RestoreFm5CatalogOnlyHold"/>).
    /// </summary>
    private async Task PrepareExtraOptionsModsAsync(AzimuthInstall install, bool hardcore, string game = ModManifest.DefaultGame)
    {
        var inputs = ExtraOptionsLaunchInputsFor(game);
        _fm5LaunchState = await Task.Run(() => PrepareExtraOptionsMods(install, hardcore, inputs));
    }

    /// <summary>What <see cref="PrepareExtraOptionsMods(AzimuthInstall, bool, ExtraOptionsLaunchInputs)"/> needs of the view model, read on the UI thread.</summary>
    private sealed record ExtraOptionsLaunchInputs(string Game, HashSet<string> Allowed, string[] LoadFirst, string HeldFile, string[] StateDirectories,
        Func<ManagedModFiles>? Factory);

    private ExtraOptionsLaunchInputs ExtraOptionsLaunchInputsFor(string game) => new(game, HardcoreSafeFolders(game), LoadFirstFolders(game),
        Path.Combine(_stateDirectory, HardcoreHeldFileFor(game)), [_stateDirectory, ModHome], game == "fm5" ? ModManagerFactory() : null);

    /// <summary>Tests only: the managed thread the last launch preparation's file work ran on (review 3.1.1: never the dispatcher's).</summary>
    internal int LaunchPreparationThreadForTests { get; private set; }

    /// <summary>
    /// The file work of the launch preparation (FM5's held folders, its receipts read, the load order): no view-model state but
    /// <paramref name="inputs"/> and the log, so it runs on a worker (review 3.1.1: never on the dispatcher). Returns FM5's state read.
    /// </summary>
    private Fm5LaunchState? PrepareExtraOptionsMods(AzimuthInstall install, bool hardcore, ExtraOptionsLaunchInputs inputs)
    {
        LaunchPreparationThreadForTests = Environment.CurrentManagedThreadId;
        Fm5LaunchState? fm5 = null;
        try
        {
            var allowed = inputs.Allowed;
            if (inputs.Game == "fm5")
            {
                foreach (var note in RestoreFm5CatalogOnlyHold(install.Root, inputs.StateDirectories)) Log(note);
                fm5 = ReadFm5LaunchState(inputs.Factory, inputs.Game);
                // unreadable receipts: Hardcore keeps only what the page last read as safe (never more), Noob changes nothing
                if (fm5 is { } state) allowed = state.BothModes;
                else Log("extraoptions mods: FM5 mod state unreadable; Hardcore keeps the mods last read as safe");
            }
            foreach (var note in ExtraOptionsMods.PrepareForLaunch(install.Root, hardcore, allowed, inputs.HeldFile, inputs.LoadFirst, inputs.Game))
                Log(note);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException or ModException)
        { Log("extraoptions mods: launch preparation skipped (" + e.GetType().Name + ")"); }
        return fm5;
    }
    /// <summary>Why the move to the per-game mods folders stopped (OD-233), for Play; null once it is done.</summary>
    private string? _modsLayoutFailure;

    /// <summary>
    /// OD-233, before every launch (any game) and when the mods are first read: the old flat <c>mods\</c> moves to the per-game folders
    /// (<see cref="ManagedModFiles.MigrateLayout"/>). A folder's game is its installed manifest's, else its catalog entry's, else FH2's. A stop
    /// (a conflict, a link, a refused move) is shown with its reason and blocks Play until it is resolved; nothing is lost either way.
    /// </summary>
    /// <summary>Where every run keeps its copy of the old <c>mods\loadorder.txt</c>: the launcher's state, never the mods bucket.</summary>
    private string LayoutBackup => Path.Combine(_stateDirectory, "mods-layout");

    private Dictionary<string, string> LayoutCatalog() => _manifestCatalog.ToDictionary(m => m.Id, m => m.ModsGame, StringComparer.OrdinalIgnoreCase);

    /// <summary>One run of the move at a time; a mod operation waits for a running one (<see cref="RunModOperationAsync"/>).</summary>
    private readonly SemaphoreSlim _layoutGate = new(1, 1);

    /// <summary>The job key of the move (its toast, its PLAY hold).</summary>
    internal const string ModsLayoutJob = "mods-layout";

    /// <summary>
    /// The one place the per-game move runs (start-up, the catalog, Recover, "Move mods now", both launches): one run at a time, its file
    /// work off the dispatcher (<see cref="Task.Run(Action)"/>). Nothing to move (the usual case) shows nothing; a real move is a job of
    /// <see cref="Jobs"/> that holds PLAY (should_block_play, owner 2026-11-10): a bottom-right toast when <paramref name="background"/>
    /// (a stop stays in it with the reason and Retry, and on Mods and Play), else the screen lock (a launch's own run), or no indicator of
    /// its own when <paramref name="inline"/> (already inside a mod operation's lock: "Move mods now").
    /// </summary>
    /// <param name="receiptsOnly">
    /// Without the catalog: only the folders the receipts name move (<see cref="ManagedModFiles.MigrateLayout"/>), so no installed mod is
    /// ever read as an unreadable record of the old layout; a folder copied by hand waits for the catalog (or Play).
    /// </param>
    /// <param name="background">
    /// Not asked for by the player: the bucket lock held for a moment (start-up retention, another launcher) defers the run instead of
    /// leaving "another launcher is changing mods" on Play; the next run (the catalog, Play) moves them.
    /// </param>
    /// <returns>False when the move stopped: a launch must not start (the reason is in <see cref="_modsLayoutFailure"/>).</returns>
    private async Task<bool> PrepareModsLayoutAsync(bool receiptsOnly, bool background, bool inline = false)
    {
        if (ModManagerFactory() is not { } factory) return true;
        var backup = LayoutBackup;
        // the cheap check (and the inline move) one at a time with a running move
        await _layoutGate.WaitAsync();
        try
        {
            if (await Task.Run(() => NothingToMove(factory)) is { } nothing) return ApplyLayoutRun(nothing, background);
            if (inline)
            {
                var catalog = LayoutCatalog();
                return ApplyLayoutRun(await Task.Run(() => MoveModsLayout(factory, catalog, backup, receiptsOnly)), background);
            }
        }
        finally { _layoutGate.Release(); }
        // the move itself is the job; every attempt (the first, a joiner's, a toast's Retry) runs it under the same gate, and every caller,
        // a joiner of a running move included, gets that attempt's real outcome (Job.Finished), never its own guess
        var attempts = 0;
        var job = Jobs.Start(new JobSpec(ModsLayoutJob, S["jobs.mods_layout"], background ? JobPresentation.Background : JobPresentation.Lock,
            ShouldBlockPlay: true, async _ =>
            {
                bool ok;
                await _layoutGate.WaitAsync();
                try
                {
                    // also a toast's Retry: the catalog as it is then
                    var catalog = LayoutCatalog();
                    var delay = LayoutMoveDelayForTests;
                    ok = ApplyLayoutRun(await Task.Run(async () =>
                    {
                        if (delay > TimeSpan.Zero) await Task.Delay(delay);
                        return MoveModsLayout(factory, catalog, backup, receiptsOnly);
                    }), background);
                }
                finally { _layoutGate.Release(); }
                if (++attempts > 1) await RefreshModsAsync(); // a Retry: the Mods page read again after it
                // a stop is the job's failure: a toast keeps it with its reason and Retry, a lock ends and the launch that ran it says why
                if (!ok) throw new JobFailedException(_modsLayoutFailure ?? S["jobs.failed"]);
            }));
        if (await job.Finished == JobState.Succeeded) return true;
        // the move stopped (its reason is set), or ended on an unexpected error: never a silent pass, Play and Mods say so
        if (_modsLayoutFailure is null) SetModsLayoutFailure(job.Error ?? S["jobs.failed"]);
        return false;
    }

    /// <summary>Tests only: how long each attempt of the move waits (off the dispatcher) before it runs, so a check can join or retry it.</summary>
    internal TimeSpan LayoutMoveDelayForTests { get; set; }

    /// <summary>The cheap check first (no lock, no toast): the per-game layout already, or the error reading it; null when a move is due.</summary>
    private static LayoutRun? NothingToMove(Func<ManagedModFiles> factory)
    {
        try { return factory().LayoutMigrationPending ? null : new([], null); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return new([], error); }
    }

    /// <summary>What one run of the move did: its log lines, and the error it stopped with (null: done, or nothing to move).</summary>
    private sealed record LayoutRun(IReadOnlyList<string> Notes, Exception? Error);

    /// <summary>The move itself; no view-model state, so it runs on any thread.</summary>
    private static LayoutRun MoveModsLayout(Func<ManagedModFiles>? factory, Dictionary<string, string> catalog, string backup, bool receiptsOnly)
    {
        if (factory is null) return new([], null);
        try
        {
            var manager = factory();
            if (!manager.LayoutMigrationPending) return new([], null);
            return new(manager.MigrateLayout(folder => catalog.TryGetValue(folder, out var game) ? game : null, backup, receiptsOnly).Notes, null);
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return new([], error); }
    }

    private bool ApplyLayoutRun(LayoutRun run, bool background)
    {
        foreach (var note in run.Notes) Log(note);
        switch (run.Error)
        {
            case null:
                SetModsLayoutFailure(null);
                return true;
            case ExtraOptionsLayoutException error:
                Log("mods layout: stopped at " + error.Folder + " (" + error.Reason + "): " + error.Message);
                SetModsLayoutFailure(S.Format("mods.layout." + error.Reason, ("folder", error.Folder)));
                return false;
            case ModException { Code: ModErrors.Busy } when background:
                Log("mods layout: deferred (busy)");
                return true;
            case var error:
                Log("mods layout: not moved: " + error.GetType().Name + (error is ModException mod ? " " + mod.Code : "") + ": " + error.Message);
                // the real cause: an interrupted operation to recover, the game running, or the state error's own text
                SetModsLayoutFailure(error switch
                {
                    ModException { Code: ModErrors.GameRunning } => S["mods.layout.game_running"],
                    ModException { Code: ModErrors.Recovery } => S["mods.play_blocked"],
                    _ => ModErrorText(error),
                });
                return false;
        }
    }

    private void SetModsLayoutFailure(string? text)
    {
        if (_modsLayoutFailure == text) return;
        _modsLayoutFailure = text;
        Changed(nameof(ModPlayNotice)); Changed(nameof(ShowModPlayNotice)); Changed(nameof(CanPlay));
    }
}