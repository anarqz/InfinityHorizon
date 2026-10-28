// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Api;
using Infinity.Core.Saves;

namespace Infinity.App;

/// <summary>
/// Owner 2026-10-28: "When creating a NOOB profile, choose to start from a hardcore one (same way we can create a hardcore from a
/// noob)". A new Noob profile may start from one of the player's Hardcore careers: that profile's newest sealed save on this PC is
/// copied into the new profile's folder (<see cref="AzimuthSaves.Clone"/>, as the save picker does: the Hardcore folder is only read,
/// whatever the copy shadows is archived first), and the Noob line's first Play uploads it. No check: a Noob line takes any save
/// (B2 closes this PC's saves to Hardcore only), so nothing here reaches the server's CR-S52 seed.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _newNoobFromHardcore;
    private SeedSourceRow? _newNoobSource;

    /// <summary>The Hardcore profiles of the chosen game with a sealed save on this PC (their own folders), newest played first.</summary>
    public IReadOnlyList<SeedSourceRow> NoobSources
    {
        get
        {
            if (_install is null) return [];
            IReadOnlyList<SaveProfile> saves;
            try { saves = AzimuthSaves.List(AzimuthSaves.SavesRoot(_install.Root)); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return []; }
            var game = _gate.Game ?? "";
            return [.. saves.Select(s => Profiles.FirstOrDefault(p => p.Mode == "hardcore" && p.Game == game && HardcoreSave(p) == s.Folder))
                .OfType<Profile>().DistinctBy(p => p.ProfileId).Select(p => new SeedSourceRow(p))];
        }
    }

    /// <summary>The folder a Hardcore profile row plays (its XUID's <c>User_</c> folder), or null.</summary>
    private static string? HardcoreSave(Profile profile) =>
        ulong.TryParse(profile.Xuid, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var xuid)
            ? AzimuthSaves.FolderFor(xuid) : null;

    /// <summary>The form's "Start from one of my Hardcore profiles" (Noob chosen, a Hardcore career on this PC to copy).</summary>
    public bool ShowNoobSeedOption => NewNoob && NoobSources.Count > 0;

    public bool NewNoobFromHardcore
    {
        get => _newNoobFromHardcore && ShowNoobSeedOption;
        set
        {
            _newNoobFromHardcore = value;
            if (value && _newNoobSource is null && NoobSources is [var only]) _newNoobSource = only;
            ChangedNoobSeedForm();
        }
    }

    public SeedSourceRow? NewNoobSource
    {
        get => _newNoobSource is { } chosen && NoobSources.Contains(chosen) ? chosen : null;
        set
        {
            // the list is rebuilt on every read: an equal row re-selected is no change (see NewSeedSource)
            if (Equals(_newNoobSource, value)) return;
            _newNoobSource = value;
            Changed(nameof(NewNoobSource));
        }
    }

    private void ChangedNoobSeedForm()
    {
        Changed(nameof(ShowNoobSeedOption));
        Changed(nameof(NewNoobFromHardcore));
        Changed(nameof(NewNoobSource));
        Changed(nameof(NoobSources));
    }

    /// <summary>
    /// After the Noob profile <paramref name="target"/> was created: copies <paramref name="source"/>'s newest sealed save into it through
    /// the save picker's clone. False when that save is no longer on this PC (the picker then opens as usual).
    /// </summary>
    private async Task<bool> StartNoobFromHardcoreAsync(Profile target, Profile source)
    {
        if (_install is null || HardcoreSave(source) is not { } folder) return false;
        SaveProfile? save;
        try { save = AzimuthSaves.List(AzimuthSaves.SavesRoot(_install.Root)).FirstOrDefault(s => s.Folder == folder); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("saves: Hardcore career not read: " + e.Message); return false; }
        if (save is null) { Log("saves: the Hardcore career has no sealed save on this PC"); return false; }
        Log("saves: the new Noob profile starts from a Hardcore career (" + folder + ")");
        _saveTarget = target;
        await PickSaveAsync(Describe(save));
        return true;
    }
}
