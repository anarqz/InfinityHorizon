// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using Infinity.Core.Api;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// OD-118: profiles are created on this PC (the same screen, no cloud call) and kept in <see cref="LocalProfileStore"/>. Offline,
/// the picker lists them; signed in, the account's profiles plus the local ones that are not uploaded yet (offline-only until the
/// update panel uploads them). A row's <see cref="Profile"/> stands for the local profile: its id is the local id and its XUID the
/// identity an offline session plays (<see cref="LocalProfile.OfflinePlayXuid"/>).
/// </summary>
public sealed partial class ShellViewModel
{
    private LocalProfileStore LocalProfiles => new(_stateDirectory);
    private LocalProfile? _chosenLocal;
    private readonly Dictionary<string, LocalProfile> _rowLocals = new(StringComparer.Ordinal);

    /// <summary>The local profile behind the chosen row (offline, or a local-only one while signed in), or null for an account row.</summary>
    internal LocalProfile? ChosenLocalProfile => _chosenLocal;

    /// <summary>A save folder exists already (CR-L36: an offline identity never reuses one).</summary>
    /// <summary>Azimuth keeps every profile's save in <c>&lt;saves&gt;\player-1\User_&lt;hex&gt;</c> (AzimuthSaves.List).</summary>
    private bool SaveFolderExists(string name) => _install is { } install && Directory.Exists(Path.Combine(AzimuthSaves.SavesRoot(install.Root), "player-1", name));

    /// <summary>The row of a local profile: the picker's <see cref="Profile"/> shape, never sent anywhere.</summary>
    /// <summary>OD-118: offline the create button makes a profile on this PC, not on an account.</summary>
    public string CreateProfileLabel => S[OfflineMode ? "offline.create_profile" : "profiles.add"];

    /// <summary>The Supporter's Noob note is for signed-in players; offline has its own line (offline.create_noob).</summary>
    public bool ShowFreeNoobHint => !CanChooseHardcore && !OfflineMode;

    private static Profile AsRow(LocalProfile local) => new(local.Id, local.Game, local.OfflinePlayXuid.ToString(CultureInfo.InvariantCulture),
        local.DisplayName, local.Name, local.Mode, local.LocalOnly ? "local" : local.NotOnAccount ? "not_on_account" : "active", local.CreatedAt, null);

    /// <summary>The rows offline: every local profile of the game (account ones cached from earlier sign-ins included).</summary>
    private IEnumerable<(Profile Row, LocalProfile? Local)> OfflineRows(string game) =>
        LocalProfiles.Read().Where(p => p.Game == game).Select(p => (AsRow(p), (LocalProfile?)p));

    /// <summary>Signed in: the local profiles not on this account (uploadable or of another account), shown as offline-only.</summary>
    private IEnumerable<(Profile Row, LocalProfile? Local)> LocalOnlyRows(string game, string accountId) =>
        LocalProfiles.Read().Where(p => p.Game == game && !p.OnAccount(accountId) && (p.LocalOnly || p.NotOnAccount)).Select(p => (AsRow(p), (LocalProfile?)p));

    private string LocalLine(LocalProfile local) => S[local.LocalOnly ? "offline.profile_local" : local.NotOnAccount ? "offline.profile_not_on_account" : "offline.profile_cached"];

    /// <summary>
    /// The new profile's name is Azimuth's own player (<see cref="AzimuthProfile"/>, owner 2026-10-26 "Adopt the Azimuth career"): a Noob
    /// profile then starts from that career (a copy; the original stays), and a Hardcore one, which starts fresh, needs another name.
    /// </summary>
    public bool ShowAzimuthNameWarning => _install is { } install && ProfileName.IsValid(NewName.Trim())
        && AzimuthProfile.SameName(AzimuthProfile.PlayerName(install.Root), NewName);

    public string AzimuthNameWarning => S[NewHardcore ? "profiles.azimuth_name_hardcore" : "profiles.azimuth_name_noob"];

    /// <summary>Creates a profile on this PC; signed in, it is uploaded at once through the same idempotent upsert as the panel.</summary>
    private async Task CreateLocalProfileAsync(string game)
    {
        // CR-L36: an offline identity never takes a save folder that exists, so the game's folder must be known before one is chosen.
        if (_install is null) { Message = S["play.setup_needed"]; return; }
        var hardcore = NewHardcore;
        var adoptsAzimuth = ShowAzimuthNameWarning;
        if (hardcore && adoptsAzimuth) { Message = S["profiles.azimuth_name_hardcore"]; return; }
        // CR-S52: the source is chosen on the form; the check runs once the Hardcore profile exists on the server
        var seedSource = hardcore && NewSeedFromProfile ? NewSeedSource?.Profile : null;
        if (hardcore && NewSeedFromProfile && seedSource is null) { Message = S["seed.pick_source"]; return; }
        // owner 2026-10-28: a Noob profile may start from one of the player's Hardcore careers on this PC (a copy)
        var noobSource = !hardcore && NewNoobFromHardcore ? NewNoobSource?.Profile : null;
        if (!hardcore && NewNoobFromHardcore && noobSource is null) { Message = S["seed.pick_source"]; return; }
        LocalProfile created;
        try { created = LocalProfiles.Create(game, NewName, hardcore ? "hardcore" : "noob", SaveFolderExists, DateTimeOffset.UtcNow, SignedIn ? _session?.Me?.AccountId : null); }
        catch (LocalProfileException refused) { Message = ProfileErrorText(refused.Code); return; }
        catch (IOException) { Message = S["offline.profile_save_failed"]; return; }
        NewName = string.Empty;
        NewSeedFromProfile = false;
        NewNoobFromHardcore = false;
        Message = S.Format("offline.profile_created", ("name", created.Name));
        if (_session is { } session && SignedIn)
        {
            await Run(async () =>
            {
                var api = new ApiProfileAccount(session.Api);
                var access = await session.Tokens.AccessAsync(CancellationToken.None);
                var plan = await ProfileUpsert.PlanAsync(api, access, LocalProfiles, game, SaveFolderExists, CancellationToken.None);
                var result = (await ProfileUpsert.UploadAsync(api, access, LocalProfiles, plan, [created.Id], CancellationToken.None)).SingleOrDefault();
                await session.RefreshMeAsync();
                Message = result is { Uploaded: false, ErrorCode: { } code } ? ProfileErrorText(code)
                    : S.Format(hardcore ? "modes.created_hardcore" : "profiles.created", ("name", result?.Profile.DisplayName ?? created.Name));
                // OD-120: a name taken meanwhile (or no room) opens the panel, which asks for a new name or keeps it on this PC.
                if (result is { Uploaded: false }) await OfferProfileUpsertAsync();
                // CR-S52: the profile exists on the server now: the dry run of the chosen source, shown before anything is copied
                else if (seedSource is not null && result?.Profile.Cloud is { } cloud
                    && session.Me?.Profiles.FirstOrDefault(p => p.ProfileId == cloud.ProfileId && p.Mode == "hardcore") is { } target)
                {
                    FillProfiles();
                    await CheckSeedAsync(target, seedSource, fromPlay: false);
                }
            });
        }
        FillProfiles();
        if (!hardcore && LocalProfiles.Read().FirstOrDefault(p => p.Id == created.Id) is { } stored)
        {
            // the player's explicit choice first: the Hardcore career picked on the form
            if (noobSource is not null && await StartNoobFromHardcoreAsync(AsRow(stored), noobSource)) return;
            // the name says it: this is the Azimuth career, copied into the new profile's folder without asking again
            if (adoptsAzimuth && await AdoptAzimuthCareerAsync(AsRow(stored))) return;
            OfferSaves(AsRow(stored)); // a Hardcore career starts empty (OD-60), never from a local save
        }
    }

    /// <summary>
    /// Copies Azimuth's own career (<c>User_9000000000000</c>, newest sealed revision) into <paramref name="target"/>'s folder through
    /// <see cref="AzimuthSaves.Clone"/>: the original is never written and every revision it shadows is archived first. False when
    /// there is no such career to adopt (the picker then opens as usual).
    /// </summary>
    private async Task<bool> AdoptAzimuthCareerAsync(Profile target)
    {
        if (_install is null || ParseXuid(target) is not { } xuid) return false;
        SaveProfile? azimuth;
        try { azimuth = AzimuthSaves.List(AzimuthSaves.SavesRoot(_install.Root), AzimuthSaves.FolderFor(xuid)).FirstOrDefault(s => s.IsOffline); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("saves: Azimuth career not read: " + e.Message); return false; }
        if (azimuth is null) { Log("saves: no Azimuth career to adopt"); return false; }
        _saveTarget = target;
        await PickSaveAsync(Describe(azimuth));
        return true;
    }

    /// <summary>
    /// B1: the save an online start of <paramref name="profile"/> falls back to when the hook cannot set the online identity: the
    /// profile's own offline identity (OD-120: a Noob account profile plays its account folder, a Hardcore one its local folder), never
    /// Azimuth's. Null for a Hardcore profile this PC has no local entry for: the hook then ends the game before it reads a save.
    /// </summary>
    internal (string Xuid, string Gamertag)? FallbackIdentityFor(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        LocalProfile? local = null;
        try { local = LocalProfiles.Read().FirstOrDefault(p => p.Cloud?.ProfileId == profile.ProfileId && p.Game == profile.Game); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("play: local profiles not read for the fallback identity: " + e.GetType().Name); }
        if (local is not null) return (local.OfflinePlayXuid.ToString(CultureInfo.InvariantCulture), local.Name);
        var name = ProfileName.Playable(profile.Gamertag);
        return profile.Mode == "hardcore" || !ProfileName.IsValid(name) ? null : (profile.Xuid, name);
    }

    /// <summary>The account plane's profile refusals (and the local store's, which use the same codes), in the player's words.</summary>
    internal string ProfileErrorText(string code) => code switch
    {
        ErrorCodes.AccountsProfileLimit => S["offline.error.profile_limit"],
        ErrorCodes.AccountsNameUnavailable => S["offline.error.name_unavailable"],
        ErrorCodes.AccountsInvalidName => S["profiles.name_rule"],
        ErrorCodes.AccountsRegistrationDisabled => S["offline.error.registration_disabled"],
        ErrorCodes.RequestIdempotencyKeyReused => S["offline.error.key_reused"],
        ErrorCodes.AccountsFrozen => S["offline.error.frozen"],
        _ => KnownErrorText(code) ?? S["offline.error.other"], // OD122-L7: any server code in the player's language
    };
}
