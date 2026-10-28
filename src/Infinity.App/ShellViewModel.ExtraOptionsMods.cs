// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using Infinity.Core.Mods;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>One folder of <c>&lt;Azimuth&gt;\mods</c> on the Mods page (OD-126): what it holds and what the last game run said.</summary>
public sealed record ExtraOptionsModItem(string Folder, string Name, string Details, string Status, bool Attention, bool Enabled, bool CanToggle)
{
    public string Toggle { get; init; } = "";
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
        if (_install is not { } install) { ExtraOptionsModItems.Clear(); Changed(nameof(HasExtraOptionsMods)); return; }
        IReadOnlyList<ExtraOptionsMod> mods; IReadOnlyDictionary<string, ExtraOptionsModStatus> status;
        try { (mods, status) = await Task.Run(() => { var m = ExtraOptionsMods.Scan(install.Root); return (m, ExtraOptionsMods.ReadStatus(install.Root, m)); }); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException)
        { Log("extraoptions mods: not read (" + e.GetType().Name + ")"); mods = []; status = new Dictionary<string, ExtraOptionsModStatus>(); }
        ExtraOptionsModItems.Clear();
        // Owner 2026-10-28: the launcher's own mods (catalog or custom) have their cards; this list is what was put there by hand
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
        { Toggle = S[mod.Enabled ? "mods.disable_action" : "mods.enable_action"] };
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

    /// <summary>The <c>mods\</c> folders of the mods the launcher installed (any catalog or custom mod, on or off).</summary>
    private HashSet<string> ManagedExtraOptionsFolders() =>
        _modHealth.Installed.Select(i => i.Manifest).OfType<ModManifest>()
            .Select(ExtraOptionsMods.FolderOf).OfType<string>().ToHashSet(StringComparer.OrdinalIgnoreCase);

    /// <summary>
    /// The <c>mods\</c> folders Hardcore keeps: installed, enabled catalog mods that change nothing for other players. A folder
    /// copied by hand, or a catalog mod that affects multiplayer, plays only outside Hardcore.
    /// </summary>
    private HashSet<string> HardcoreSafeFolders() =>
        _modFacts.Select(f => f.Installed is { Enabled: true, Problem: null } installed ? installed.Manifest ?? f.Manifest : null)
            .Where(m => m is not null && !ModMultiplayer.AffectsMultiplayer(m))
            .Select(m => ExtraOptionsMods.FolderOf(m!)).OfType<string>().ToHashSet(StringComparer.OrdinalIgnoreCase);

    /// <summary>
    /// Before a launch: <c>loadorder.txt</c> puts every mod after its dependencies; Hardcore online plays only with
    /// <see cref="HardcoreSafeFolders"/>, and any other launch gets back what it held.
    /// </summary>
    private void PrepareExtraOptionsMods(AzimuthInstall install, bool hardcore)
    {
        try
        {
            foreach (var note in ExtraOptionsMods.PrepareForLaunch(install.Root, hardcore, HardcoreSafeFolders(),
                         Path.Combine(_stateDirectory, HardcoreHeldFile)))
                Log(note);
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException)
        { Log("extraoptions mods: launch preparation skipped (" + e.GetType().Name + ")"); }
    }
}
