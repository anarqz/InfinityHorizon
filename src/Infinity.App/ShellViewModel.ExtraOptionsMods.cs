// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using Infinity.Core.Matchmaking;
using Infinity.Core.Mods;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>One folder of <c>&lt;Azimuth&gt;\mods</c> on the Mods page (OD-126): what it holds and what the last game run said.</summary>
public sealed record ExtraOptionsModItem(string Folder, string Name, string Details, string Status, bool Attention, bool Enabled, bool CanToggle)
{
    public string Toggle { get; init; } = "";
    /// <summary>Owner 2026-10-28: a manual mod, once off, can be deleted from the mods folder (asked twice).</summary>
    public bool CanDelete { get; init; }
    public string DeleteLabel { get; init; } = "";
}

public sealed partial class ShellViewModel
{
    /// <summary>Remembers the mods a Hardcore launch switched off, to switch them back on at the next other launch.</summary>
    private const string HardcoreHeldFile = "extraoptions-hardcore-held.json";

    public ObservableCollection<ExtraOptionsModItem> ExtraOptionsModItems { get; } = [];
    public bool HasExtraOptionsMods => ExtraOptionsModItems.Count > 0;
    public string ExtraOptionsModsFolder => _install is null ? "" : S.Format("eomods.folder", ("path", ExtraOptionsMods.ModsDirectory(_install.Root)));
    public string ExtraOptionsModsHint => S[ProfileHardcore ? "eomods.hint_hardcore" : "eomods.hint"];

    /// <summary>Reads <c>mods\</c>, <c>loadorder.txt</c> and the last <c>FH2_ExtraOptions.log</c> off the UI thread.</summary>
    private async Task RefreshExtraOptionsModsAsync()
    {
        // W11: the mods folder and its load order are FH2's; a game without mods of its own lists none
        if (_install is not { } install || !CurrentGame.FullFeatures) { ExtraOptionsModItems.Clear(); Changed(nameof(HasExtraOptionsMods)); return; }
        IReadOnlyList<ExtraOptionsMod> mods; IReadOnlyDictionary<string, ExtraOptionsModStatus> status;
        try { (mods, status) = await Task.Run(() => { var m = ExtraOptionsMods.Scan(install.Root); return (m, ExtraOptionsMods.ReadStatus(install.Root, m)); }); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException)
        { Log("extraoptions mods: not read (" + e.GetType().Name + ")"); mods = []; status = new Dictionary<string, ExtraOptionsModStatus>(); }
        ExtraOptionsModItems.Clear();
        // Owner 2026-10-28: the launcher's own mods have their cards; this list is what was put there by hand (or added from a ZIP)
        var managed = ManagedExtraOptionsFolders();
        // a folder ExtraOptions loads nothing from (what an uninstalled mod's own settings leave behind) is no mod: not listed
        foreach (var mod in mods.Where(m => !managed.Contains(m.Folder) && (m.HasMedia || m.HasDatabase || m.HasStrings || m.Plugins > 0 || m.ManifestError is not null)))
            ExtraOptionsModItems.Add(Item(mod, mods, status));
        Changed(nameof(HasExtraOptionsMods)); Changed(nameof(ExtraOptionsModsFolder)); Changed(nameof(ExtraOptionsModsHint));
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
            await Task.Run(() => ExtraOptionsMods.DeleteFolder(install.Root, item.Folder));
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
            await Task.Run(() => ExtraOptionsMods.SetEnabled(install.Root, item.Folder, !item.Enabled));
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
        try
        {
            var plan = await Task.Run(() => ExtraOptionsModSetSource.PlanFor(install.Root, cosmetic, game));
            Log("mods: lobby set planned from ExtraOptions: " + (plan.Count == 0 ? "none" : string.Join(", ", plan.Select(p => p.Entry.Id + " " + p.Entry.Version))));
            return new ExtraOptionsModSetSource(install.Root, plan, launchUtc, game);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException or ModSetException)
        {
            Log("mods: lobby set not planned (" + e.GetType().Name + ")");
            bool anyEnabled;
            try { anyEnabled = ExtraOptionsMods.Scan(install.Root, game).Any(m => m.Enabled); }
            catch (Exception scan) when (scan is IOException or UnauthorizedAccessException or InvalidDataException) { anyEnabled = true; }
            return anyEnabled ? null : VanillaModSet.Instance;
        }
    }

    /// <summary>OD-233: the launcher's FM5 mods as read for one launch: their folders (enabled by receipt), and those for both modes.</summary>
    private sealed record Fm5LaunchState(HashSet<string> Managed, HashSet<string> BothModes);

    /// <summary>The state <see cref="PrepareExtraOptionsMods"/> read for the current FM5 launch; null when it could not be read.</summary>
    private Fm5LaunchState? _fm5LaunchState;

    /// <summary>
    /// FM5's installed mods read fresh from their receipts (never the page's cached state, which may be stale or not loaded yet); null when
    /// any of it could not be read, so nothing is turned off on a guess.
    /// </summary>
    private Fm5LaunchState? ReadFm5LaunchState(string game)
    {
        if (ModManagerFactory() is not { } factory) return null;
        ModHealth health;
        try { health = factory().Inspect(); }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return null; }
        if (health.RecoveryHere || health.Problems.Count > 0 || health.Installed.Any(i => i.Problem is not null || i.Manifest is null)) return null;
        var mine = health.Installed.Where(i => i.Enabled && i.Manifest!.ModsGame == game && !i.Manifest.Retired).ToList();
        HashSet<string> Folders(IEnumerable<ModInstallation> list) => list.Select(i => ExtraOptionsMods.FolderOf(i.Manifest!)).OfType<string>().ToHashSet(StringComparer.OrdinalIgnoreCase);
        return new(Folders(mine), Folders(mine.Where(i => !ModMultiplayer.AffectsMultiplayer(i.Manifest!))));
    }

    /// <summary>
    /// Before a launch, after <see cref="PrepareModsLayout"/>: the game's <c>loadorder.txt</c> puts every mod after its dependencies (and
    /// OD-232's load-first mods on top); Hardcore plays only with <see cref="HardcoreSafeFolders"/>, and any other launch gets back what it
    /// held. FM5's folder is launcher-managed, catalog only (OD-233, <see cref="ExtraOptionsMods.KeepCatalogOnly"/>): Noob plays the
    /// launcher's enabled mods, Hardcore only those for both modes (FH2's rule); a folder the launcher turned off goes back on once allowed.
    /// </summary>
    private void PrepareExtraOptionsMods(AzimuthInstall install, bool hardcore, string game = ModManifest.DefaultGame)
    {
        try
        {
            if (game == "fm5")
            {
                foreach (var note in ExtraOptionsMods.ArrangeLoadOrder(install.Root, LoadFirstFolders(game), game)) Log(note);
                _fm5LaunchState = ReadFm5LaunchState(game);
                if (_fm5LaunchState is not { } state) { Log("extraoptions mods: FM5 mod state unreadable; nothing turned off"); return; }
                foreach (var note in ExtraOptionsMods.KeepCatalogOnly(install.Root, hardcore ? state.BothModes : state.Managed,
                             Path.Combine(_stateDirectory, "extraoptions-fm5-unmanaged-held.json"), game))
                    Log(note);
                return;
            }
            foreach (var note in ExtraOptionsMods.PrepareForLaunch(install.Root, hardcore, HardcoreSafeFolders(game),
                         Path.Combine(_stateDirectory, HardcoreHeldFile), LoadFirstFolders(game), game))
                Log(note);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException or ModException)
        { Log("extraoptions mods: launch preparation skipped (" + e.GetType().Name + ")"); }
    }
    /// <summary>Why the move to the per-game mods folders stopped (OD-233), for Play; null once it is done.</summary>
    private string? _modsLayoutFailure;

    /// <summary>
    /// OD-233, before every launch (any game) and when the mods are first read: the old flat <c>mods\</c> moves to the per-game folders
    /// (<see cref="ManagedModFiles.MigrateLayout"/>). A folder's game is its installed manifest's, else its catalog entry's, else FH2's. A stop
    /// (a conflict, a link, a refused move) is shown with its reason and blocks Play until it is resolved; nothing is lost either way.
    /// </summary>
    /// <returns>False when the move stopped: the launch must not start (the reason is in <see cref="_modsLayoutFailure"/>).</returns>
    private bool PrepareModsLayout(AzimuthInstall install)
    {
        if (ModManagerFactory() is not { } factory) return true;
        var catalog = _manifestCatalog.ToDictionary(m => m.Id, m => m.ModsGame, StringComparer.OrdinalIgnoreCase);
        try
        {
            var manager = factory();
            if (!manager.LayoutMigrationPending) { SetModsLayoutFailure(null); return true; }
            var result = manager.MigrateLayout(folder => catalog.TryGetValue(folder, out var game) ? game : null, Path.Combine(_stateDirectory, "mods-layout"));
            foreach (var note in result.Notes) Log(note);
            SetModsLayoutFailure(null);
            return true;
        }
        catch (ExtraOptionsLayoutException error)
        {
            Log("mods layout: stopped at " + error.Folder + " (" + error.Reason + "): " + error.Message);
            SetModsLayoutFailure(S.Format("mods.layout." + error.Reason, ("folder", error.Folder)));
            return false;
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error))
        {
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