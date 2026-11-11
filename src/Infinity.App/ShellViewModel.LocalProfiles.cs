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
/// identity an offline session plays (<see cref="LocalProfile.IdentityXuid"/>; its folder <see cref="LocalProfile.SaveXuid"/>).
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
    private bool SaveFolderExists(string name) => _install is { } install && Directory.Exists(Path.Combine(AzimuthSaves.SavesRoot(install.Root, CurrentGame), "player-1", name));

    /// <summary>The row of a local profile: the picker's <see cref="Profile"/> shape, never sent anywhere.</summary>
    /// <summary>OD-118: offline the create button makes a profile on this PC, not on an account.</summary>
    public string CreateProfileLabel => S[OfflineMode ? "offline.create_profile" : "profiles.add"];

    /// <summary>The "new profiles are Noob" note is for signed-in players; offline has its own line (offline.create_noob).</summary>
    public bool ShowFreeNoobHint => !CanChooseHardcore && !OfflineMode;

    private static Profile AsRow(LocalProfile local) => new(local.Id, local.Game, local.SaveXuid.ToString(CultureInfo.InvariantCulture),
        local.DisplayName, local.Name, local.Mode, local.LocalOnly ? "local" : local.NotOnAccount ? "not_on_account" : "active", local.CreatedAt, null);

    /// <summary>The rows offline: every local profile of the game (account ones cached from earlier sign-ins included).</summary>
    private IEnumerable<(Profile Row, LocalProfile? Local)> OfflineRows(string game) =>
        LocalProfiles.Read().Where(p => p.Game == game).Select(p => (AsRow(p), (LocalProfile?)p));

    /// <summary>Signed in: the local profiles not on this account (uploadable or of another account), shown as offline-only.</summary>
    private IEnumerable<(Profile Row, LocalProfile? Local)> LocalOnlyRows(string game, string accountId) =>
        LocalProfiles.Read().Where(p => p.Game == game && !p.OnAccount(accountId) && (p.LocalOnly || p.NotOnAccount)).Select(p => (AsRow(p), (LocalProfile?)p));

    private string LocalLine(LocalProfile local) => S[local.LocalOnly ? "offline.profile_local" : local.NotOnAccount ? "offline.profile_not_on_account" : "offline.profile_cached"];

    /// <summary>
    /// A Hardcore profile named as Azimuth's own player (<see cref="AzimuthProfile"/>) needs another name: it starts fresh, and the
    /// name is Azimuth's career. An Azimuth profile plays that career itself (OD-239), so it takes any name and nothing is copied.
    /// </summary>
    public bool ShowAzimuthNameWarning => NewHardcore && _install is { } install && ProfileName.IsValid(NewName.Trim())
        && AzimuthProfile.SameName(AzimuthProfile.PlayerName(install.Root), NewName);

    public string AzimuthNameWarning => S["profiles.azimuth_name_hardcore"];

    /// <summary>Creates a profile on this PC; signed in, it is uploaded at once through the same idempotent upsert as the panel.</summary>
    private async Task CreateLocalProfileAsync(string game)
    {
        // CR-L36: an offline identity never takes a save folder that exists, so the game's folder must be known before one is chosen.
        if (_install is null) { Message = S["play.setup_needed"]; return; }
        var hardcore = NewHardcore;
        if (ShowAzimuthNameWarning) { Message = S["profiles.azimuth_name_hardcore"]; return; }
        // CR-S52: the source is chosen on the form; the check runs once the Hardcore profile exists on the server
        var seedSource = hardcore && NewSeedFromProfile ? NewSeedSource?.Profile : null;
        if (hardcore && NewSeedFromProfile && seedSource is null) { Message = S["seed.pick_source"]; return; }

        // OD-240: one Azimuth identity per account: signed in with one, no second Azimuth profile is made (here or on the server)
        if (!hardcore && SignedIn && _session?.Me is { } me && AzimuthIdentity.Exists(me, game)) { Message = S[AzimuthIdentity.ExistsKey]; return; }
        LocalProfile created;
        try { created = LocalProfiles.Create(game, NewName, hardcore ? "hardcore" : "noob", SaveFolderExists, DateTimeOffset.UtcNow, SignedIn ? _session?.Me?.AccountId : null); }
        catch (LocalProfileException refused) { Message = ProfileErrorText(refused.Code); return; }
        catch (IOException) { Message = S["offline.profile_save_failed"]; return; }
        NewName = string.Empty;
        NewSeedFromProfile = false;
        Message = S.Format(hardcore ? "offline.profile_created" : "profiles.azimuth_identity_made", ("name", created.Name));
        if (_session is { } session && SignedIn)
        {
            await Run(S.Format("busy.creating_profile", ("mode", BusyMode(hardcore ? "hardcore" : "noob"))), async () =>
            {
                var api = new ApiProfileAccount(session.Api, session.CareerGame(CurrentGame.Id));
                var access = await session.Tokens.AccessAsync(CancellationToken.None);
                var plan = await ProfileUpsert.PlanAsync(api, access, LocalProfiles, game, SaveFolderExists, CancellationToken.None);
                var result = (await ProfileUpsert.UploadAsync(api, access, LocalProfiles, plan, [created.Id], CancellationToken.None)).SingleOrDefault();
                await session.RefreshMeAsync();
                // OD-240: PROFILE_LIMIT on an Azimuth create means the account already has its identity (made on another PC meanwhile): said
                // plainly, never retried under another name; the refresh makes this PC's profile that identity (no second one on the server).
                if (!hardcore && result is null or { Uploaded: false, ErrorCode: ErrorCodes.AccountsProfileLimit })
                {
                    await ProfileUpsert.PlanAsync(api, access, LocalProfiles, game, SaveFolderExists, CancellationToken.None);
                    Message = S[AzimuthIdentity.ExistsKey];
                    return;
                }
                Message = result is { Uploaded: false, ErrorCode: { } code } ? ProfileErrorText(code)
                    : S.Format(hardcore ? "modes.created_hardcore" : "profiles.azimuth_identity_made", ("name", result?.Profile.DisplayName ?? created.Name));
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
        FillProfiles(); // OD-239: an Azimuth profile plays Azimuth's own save; a Hardcore career starts empty (OD-60); nothing is copied
    }

    /// <summary>
    /// B1: the save an online start of <paramref name="profile"/> falls back to when the hook cannot set the online identity: the
    /// profile's own offline identity (OD-120: a Noob account profile plays its account folder, a Hardcore one its local folder), never
    /// Azimuth's identity. Null for a Hardcore profile this PC has no local entry for: the hook then ends the game before it reads a save.
    /// The mode goes with it (OD-239): an Azimuth profile's fallback plays Azimuth's own save, as its online session would.
    /// </summary>
    internal (string Xuid, string Gamertag, string Mode)? FallbackIdentityFor(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        LocalProfile? local = null;
        try { local = LocalProfiles.Read().FirstOrDefault(p => p.Cloud?.ProfileId == profile.ProfileId && p.Game == profile.Game); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { Log("play: local profiles not read for the fallback identity: " + e.GetType().Name); }
        if (local is not null) return (local.IdentityXuid.ToString(CultureInfo.InvariantCulture), local.Name, profile.Mode);
        var name = ProfileName.Playable(profile.Gamertag);
        return profile.Mode == "hardcore" || !ProfileName.IsValid(name) ? null : (profile.Xuid, name, profile.Mode);
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
