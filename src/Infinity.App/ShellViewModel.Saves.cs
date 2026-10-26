// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Globalization;
using System.IO;
using Infinity.Core.Api;
using Infinity.Core.Shell;
using Infinity.Core.Saves;

namespace Infinity.App;

/// <summary>One Azimuth profile the picker offers (OD-41).</summary>
public sealed record SaveChoice(SaveProfile Save, string Title, string Detail);

/// <summary>
/// OD-41: a new Infinity profile starts from a save the player picks among Azimuth's own profiles
/// on this PC. The pick is backed up and cloned into the profile's container
/// (<see cref="AzimuthSaves.Clone"/>); the original is never written.
/// </summary>
public sealed partial class ShellViewModel
{
    private Profile? _saveTarget;

    public ObservableCollection<SaveChoice> SaveChoices { get; } = [];

    /// <summary>The save picker is open (it covers the profile and home panels).</summary>
    public bool ShowPickSave => _saveTarget is not null;

    public string PickSaveText => _saveTarget is null ? string.Empty : S.Format("saves.pick_text", ("name", ProfileName.Plain(_saveTarget.Gamertag)));

    public bool CanPickSave => Ready && _install is not null && !Playing && !Busy && !ChosenProfileHardcore;

    /// <summary>
    /// B2: a Hardcore career always starts fresh (OD-60; the server refuses anything else with SAVES.HARDCORE_NEEDS_FRESH_CAREER), so
    /// no save from this PC is ever offered to it: the picker would only lead to a blocked launch.
    /// </summary>
    private bool ChosenProfileHardcore => Profiles.FirstOrDefault(p => p.ProfileId == _gate.ProfileKey) is { Mode: "hardcore" };

    public bool ShowHardcoreSaveNote => Ready && ChosenProfileHardcore;

    /// <summary>Home's "use a save from this PC": the picker for the chosen profile.</summary>
    public void OpenSavePicker()
    {
        if (ChosenProfileHardcore) { Message = S["saves.hardcore_fresh"]; return; }
        if (CanPickSave && Profiles.FirstOrDefault(p => p.ProfileId == _gate.ProfileKey) is { } profile)
        {
            OfferSaves(profile, always: true);
        }
    }

    /// <summary>
    /// Opens the picker for <paramref name="profile"/> when this PC has Azimuth saves to offer
    /// (after creating a profile), or always (from Home, to show that none were found).
    /// </summary>
    private void OfferSaves(Profile profile, bool always = false)
    {
        if (_install is null)
        {
            return;
        }
        SaveChoices.Clear();
        if (ParseXuid(profile) is not { } xuid)
        {
            return;
        }
        IReadOnlyList<SaveProfile> saves;
        try
        {
            saves = AzimuthSaves.List(AzimuthSaves.SavesRoot(_install.Root), AzimuthSaves.FolderFor(xuid));
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            Message = S["saves.read_failed"] + " (" + e.Message + ")";
            return;
        }
        foreach (var save in saves)
        {
            SaveChoices.Add(Describe(save));
        }
        if (SaveChoices.Count == 0 && !always)
        {
            return;
        }
        _saveTarget = profile;
        Message = SaveChoices.Count == 0 ? S["saves.none_found"] : null;
        Changed(string.Empty);
    }

    private SaveChoice Describe(SaveProfile save)
    {
        var title = save.IsOffline ? S["saves.offline_profile"] : S.Format("saves.profile_folder", ("folder", save.Folder));
        var parts = new List<string>();
        if (save.Level is { } level)
        {
            parts.Add(S.Format("saves.level", ("level", level.ToString(CultureInfo.CurrentCulture))));
        }
        if (save.Credits is { } credits)
        {
            parts.Add(S.Format("saves.credits", ("credits", credits.ToString("N0", CultureInfo.CurrentCulture))));
        }
        parts.Add(S.Format("saves.last_played", ("when", save.LastPlayedUtc.ToLocalTime().ToString("g", CultureInfo.CurrentCulture))));
        return new SaveChoice(save, title, string.Join(" · ", parts));
    }

    /// <summary>Clones the picked save into the target profile's container, then closes the picker.</summary>
    public async Task PickSaveAsync(SaveChoice choice)
    {
        ArgumentNullException.ThrowIfNull(choice);
        if (_saveTarget is not { } target || _install is null)
        {
            return;
        }
        if (target.Mode == "hardcore") { CloseSavePicker(); Message = S["saves.hardcore_fresh"]; return; } // B2
        var handled = false;
        PreviewSavePick(choice, ref handled);
        if (handled) return;
        if (Playing || System.Diagnostics.Process.GetProcessesByName("azimuth").Length > 0)
        {
            Message = S["saves.game_running"];
            return;
        }
        var savesRoot = AzimuthSaves.SavesRoot(_install.Root);
        if (ParseXuid(target) is not { } xuid)
        {
            return;
        }
        await Run(async () =>
        {
            Message = S["saves.cloning"];
            CloneResult result;
            try
            {
                result = await Task.Run(() => AzimuthSaves.Clone(savesRoot, choice.Save, xuid, DateTime.UtcNow));
            }
            catch (SaveException e) when (e.Landed)
            {
                Log("save clone landed but did not verify: " + e.Message);
                CloseSavePicker();
                Message = S["saves.clone_unverified"] + " (" + e.Message + ")";
                return;
            }
            catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException)
            {
                Message = S["saves.clone_failed"] + " (" + e.Message + ")";
                return;
            }
            Log($"save clone {choice.Save.Folder}/{choice.Save.Revision.Number:x16} -> {result.Target}/{result.Number:x16}, {result.Backups.Count} backups in {AzimuthSaves.ArchiveDir}");
            CloseSavePicker();
            Message = S.Format("saves.cloned", ("title", choice.Title), ("name", ProfileName.Plain(target.Gamertag)));
            try
            {
                new SaveLinks(_stateDirectory).Set(target.Xuid, new SaveLink(choice.Save.Folder, choice.Save.Revision.Number, result.Number, DateTime.UtcNow));
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException)
            {
                // The link only labels the UI; the clone stands.
                Log("save link not written: " + e.Message);
            }
        });
    }

    partial void PreviewSavePick(SaveChoice choice, ref bool handled);

    /// <summary>"Start a new career": nothing is copied, the game creates the save.</summary>
    public void SkipSave()
    {
        var name = ProfileName.Plain(_saveTarget?.Gamertag);
        CloseSavePicker();
        Message = S.Format("saves.skipped", ("name", name));
    }

    private void CloseSavePicker()
    {
        _saveTarget = null;
        SaveChoices.Clear();
        Changed(string.Empty);
    }

    /// <summary>The profile's XUID (a decimal string, ACCOUNTS.md §1); null, with a message, when it is not one.</summary>
    private ulong? ParseXuid(Profile profile)
    {
        if (ulong.TryParse(profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid))
        {
            return xuid;
        }
        Message = S["saves.read_failed"] + " (xuid)";
        return null;
    }
}
