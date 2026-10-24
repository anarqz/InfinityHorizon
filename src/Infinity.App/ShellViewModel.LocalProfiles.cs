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
    private bool SaveFolderExists(string name) => _install is { } install && Directory.Exists(Path.Combine(AzimuthSaves.SavesRoot(install.Root), name));

    /// <summary>The row of a local profile: the picker's <see cref="Profile"/> shape, never sent anywhere.</summary>
    private static Profile AsRow(LocalProfile local) => new(local.Id, local.Game, local.OfflinePlayXuid.ToString(CultureInfo.InvariantCulture),
        local.DisplayName, local.Name, local.Mode, local.LocalOnly ? "local" : local.NotOnAccount ? "not_on_account" : "active", local.CreatedAt, null);

    /// <summary>The rows offline: every local profile of the game (account ones cached from earlier sign-ins included).</summary>
    private IEnumerable<(Profile Row, LocalProfile? Local)> OfflineRows(string game) =>
        LocalProfiles.Read().Where(p => p.Game == game).Select(p => (AsRow(p), (LocalProfile?)p));

    /// <summary>Signed in: the local profiles not on this account (uploadable or of another account), shown as offline-only.</summary>
    private IEnumerable<(Profile Row, LocalProfile? Local)> LocalOnlyRows(string game, string accountId) =>
        LocalProfiles.Read().Where(p => p.Game == game && !p.OnAccount(accountId) && (p.LocalOnly || p.NotOnAccount)).Select(p => (AsRow(p), (LocalProfile?)p));

    private string LocalLine(LocalProfile local) => S[local.LocalOnly ? "offline.profile_local" : local.NotOnAccount ? "offline.profile_not_on_account" : "offline.profile_cached"];

    /// <summary>Creates a profile on this PC; signed in, it is uploaded at once through the same idempotent upsert as the panel.</summary>
    private async Task CreateLocalProfileAsync(string game)
    {
        var hardcore = NewHardcore;
        LocalProfile created;
        try { created = LocalProfiles.Create(game, NewName, hardcore ? "hardcore" : "noob", SaveFolderExists, DateTimeOffset.UtcNow); }
        catch (LocalProfileException refused) { Message = ProfileErrorText(refused.Code); return; }
        catch (IOException) { Message = S["offline.profile_save_failed"]; return; }
        NewName = string.Empty;
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
            });
        }
        FillProfiles();
        if (!hardcore && LocalProfiles.Read().FirstOrDefault(p => p.Id == created.Id) is { } stored)
            OfferSaves(AsRow(stored)); // a Hardcore career starts empty (OD-60), never from a local save
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
        _ => S["offline.error.other"],
    };
}
