// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Api;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// OD-240: one Azimuth identity per account, kept behind the scenes (<see cref="AzimuthIdentity"/>). Signed in it is the account's single
/// <c>noob</c> profile (the newest played of pre-CR-S98 duplicates); offline it is this PC's one local Azimuth profile. PLAY AZIMUTH plays
/// this PC's <c>User_9000000000000</c> under it, online or offline; a game without its career yet gets one created at the click, under the
/// busy overlay. Without an identity the player picks its gamertag once (offline, Azimuth's own player name is used when it is a valid
/// gamertag, and nothing is asked).
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The Azimuth identity's row of the chosen game (signed in: the account's; offline: this PC's), null without one. No request, no file read.</summary>
    private ProfileRow? AzimuthRow()
    {
        if (!OfflineMode && _session?.Me is { } me)
        {
            if (me.Players is not null)
            {
                if (AzimuthIdentity.Of(me) is not { } player) return null;
                RememberAzimuthXuid(player.Xuid);
                return ProfileRows.FirstOrDefault(r => r.Card?.Player.ProfileId == player.ProfileId);
            }
            return AzimuthIdentity.OfRows(me.Profiles.Where(p => p.Game == _gate.Game)) is { } plain
                ? ProfileRows.FirstOrDefault(r => r.OnAccount && r.Profile.ProfileId == plain.ProfileId) : null;
        }
        if (!OfflineMode) return null;
        _azimuthXuid ??= LaunchModeStore.ReadAzimuthXuid(_stateDirectory) ?? string.Empty;
        return AzimuthIdentity.Local(_rowLocals.Values, _gate.Game ?? string.Empty, _azimuthXuid.Length > 0 ? _azimuthXuid : null) is { } local
            ? ProfileRows.FirstOrDefault(r => _rowLocals.TryGetValue(r.Profile.ProfileId, out var l) && l.Id == local.Id) : null;
    }

    /// <summary>The account identity's XUID kept for offline (read once; empty: none kept).</summary>
    private string? _azimuthXuid;

    /// <summary>The signed-in account's identity, kept on this PC when it changes, so offline plays under the same gamertag.</summary>
    private void RememberAzimuthXuid(string xuid)
    {
        if (_azimuthXuid == xuid) return;
        _azimuthXuid = xuid;
        try { LaunchModeStore.WriteAzimuthXuid(_stateDirectory, xuid); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or ArgumentException) { Log("play: the Azimuth identity was not kept for offline: " + e.GetType().Name); }
    }

    /// <summary>The identity's gamertag (what leaderboards, speed traps and liveries show for Azimuth play), null without one.</summary>
    private string? AzimuthIdentityName() => AzimuthRow()?.Gamertag;

    /// <summary>Offline without an identity: Azimuth's own player name when it is a valid gamertag (the identity is then made with it, nothing asked).</summary>
    private string? AzimuthOwnName()
    {
        if (_install is not { } install) return null;
        // read once per Azimuth folder (azimuth.ini is small, but PLAY never reads a file on navigation)
        if (_azimuthOwnName is not { } cached || cached.Root != install.Root)
            _azimuthOwnName = cached = (install.Root, AzimuthProfile.PlayerName(install.Root) is { } name && ProfileName.IsValid(name) ? name : null);
        return cached.Name;
    }

    private (string Root, string? Name)? _azimuthOwnName;

    /// <summary>
    /// PLAY AZIMUTH with an identity: its career of the chosen game is made first when it has none (signed in, under "Starting your AZIMUTH
    /// career…"), then it is chosen and the existing play path runs (its gates unchanged).
    /// </summary>
    private async Task PlayAzimuthAsync(ProfileRow row)
    {
        if (await EnsureAzimuthCareerAsync(row) is not { } profile) return; // why is in Message
        _startRow = null;
        ChooseProfile(profile);
        Log($"play: AZIMUTH {CurrentGame.Id} chosen on PLAY");
        await PlayAsync();
    }

    /// <summary>
    /// The identity's career of the chosen game: the one it has, or one made now (signed in, <c>POST /players/{xuid}/careers</c> under the busy
    /// overlay "Starting your AZIMUTH career…"; 409 <c>CAREER_EXISTS</c> continues). Null when it could not be made (why is in <see cref="Message"/>).
    /// </summary>
    internal async Task<Profile?> EnsureAzimuthCareerAsync(ProfileRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (row.Card is not { CareerRow: null } card) return row.Profile;
        Profile? created = null;
        await Run(S.Format("busy.starting_career", ("mode", BusyMode("noob")), ("game", GameLabel)), async () =>
        {
            (created, _) = await EnsureCareerAsync(card);
        });
        if (created is not null) Log($"play: AZIMUTH career of {created.Game} made behind the scenes for {created.ProfileId}");
        return created;
    }

    /// <summary>
    /// PLAY AZIMUTH without an identity. Offline: made at once from Azimuth's own player name when it is a valid gamertag, then played; else
    /// the name is asked once (Profiles' form). Signed in: the gamertag is picked once (the account's single <c>noob</c> profile, POST /profiles).
    /// </summary>
    private async Task CreateAzimuthIdentityAsync()
    {
        _createMode = "noob";
        NewHardcore = false;
        if (OfflineMode && AzimuthOwnName() is { } name && _gate.Game is { } game && _install is not null)
        {
            try
            {
                var made = LocalProfiles.Create(game, name, "noob", SaveFolderExists, DateTimeOffset.UtcNow);
                Log($"play: this PC's Azimuth identity made behind the scenes as {made.Name} ({made.Id})");
            }
            catch (LocalProfileException refused) { Message = ProfileErrorText(refused.Code); return; }
            catch (IOException) { Message = S["offline.profile_save_failed"]; return; }
            FillProfiles();
            if (AzimuthRow() is { } row) await PlayAzimuthAsync(row);
            return;
        }
        OpenProfiles();
        if (ShowSaveManager) SavesGo(SavesScreen.Create);
        Message = S["play.azimuth_pick_name"];
    }

    /// <summary>The Azimuth identity was just made from the name form: back to PLAY, where PLAY AZIMUTH plays it.</summary>
    private void AzimuthIdentityMade()
    {
        if (AzimuthRow() is null) return;
        _onProfiles = false;
        _nav.Root();
        _createMode = "hardcore";
        FillPlayBoard();
        Changed(string.Empty);
    }
}
