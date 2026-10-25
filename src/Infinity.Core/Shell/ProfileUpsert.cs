// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Saves;

namespace Infinity.Core.Shell;

/// <summary>The account calls the profile list needs (ACCOUNTS.md §5: GET /me, POST /profiles, the OD-120 free rename).</summary>
public interface IProfileAccountApi
{
    Task<Me> MeAsync(string access, CancellationToken cancellation);
    Task<Profile> CreateProfileAsync(string access, NewProfile profile, string idempotencyKey, CancellationToken cancellation);
    Task<Profile> RenameProfileAsync(string access, string xuid, NameChange change, string idempotencyKey, CancellationToken cancellation);
}

/// <summary><see cref="IProfileAccountApi"/> over <see cref="ApiClient"/>.</summary>
public sealed class ApiProfileAccount(ApiClient api) : IProfileAccountApi
{
    public Task<Me> MeAsync(string access, CancellationToken cancellation) => api.MeAsync(access, cancellation);
    public Task<Profile> CreateProfileAsync(string access, NewProfile profile, string idempotencyKey, CancellationToken cancellation) =>
        api.CreateProfileAsync(access, profile, idempotencyKey, cancellation);
    public Task<Profile> RenameProfileAsync(string access, string xuid, NameChange change, string idempotencyKey, CancellationToken cancellation) =>
        api.RenameProfileAsync(access, xuid, change, idempotencyKey, cancellation);
}

/// <summary>
/// What the "Update online profiles" panel shows after sign-in: the account's profiles (cached ones refreshed, other PCs' added),
/// the local profiles that may be uploaded, and the room left under the per-account limit. When the local ones do not all fit,
/// the player chooses which to upload; the rest stay offline-only.
/// </summary>
public sealed record ProfileUpsertPlan(string AccountId, IReadOnlyList<LocalProfile> Account, IReadOnlyList<LocalProfile> Uploadable, int Room)
{
    /// <summary>
    /// Local profiles this refresh linked to the account (an upload the server stored whose answer was lost): registered now, so
    /// their saves are continued like any registration's (<see cref="ProfileUpsert.ContinueOfflineSave"/>).
    /// </summary>
    public IReadOnlyList<LocalProfile> Recovered { get; init; } = [];

    public bool MustChoose => Uploadable.Count > Room;

    /// <summary>
    /// OD-120: what signing in registers without asking: every local profile, oldest first, when they all fit; none when the player
    /// must choose (or there is no room), which the panel asks instead.
    /// </summary>
    public IReadOnlyList<string> Automatic => MustChoose || Room == 0 ? []
        : Uploadable.OrderBy(p => p.CreatedAt, StringComparer.Ordinal).Select(p => p.Id).ToArray();

    /// <summary>The account profiles with an unused OD-120 free rename (a clash rename's notice).</summary>
    public IEnumerable<LocalProfile> RenameNotices => Account.Where(p => p.Cloud?.RenameNotice is not null);
}

/// <summary>One upload: the profile as stored afterwards, and the account plane's error code when it was refused.</summary>
public sealed record ProfileUpsertResult(LocalProfile Profile, string? ErrorCode)
{
    public bool Uploaded => ErrorCode is null;
}

/// <summary>
/// OD-118 upsert of local profiles to the account: GET /me, then POST /profiles {game, name, mode} for each chosen one with its
/// own <see cref="LocalProfile.UploadKey"/> as <c>Idempotency-Key</c> (a retry replays the first outcome and never duplicates),
/// storing the server's xuid/gamertag/profileId at once. Profiles cached for another account are never uploaded under this one.
/// </summary>
public static class ProfileUpsert
{
    public static async Task<ProfileUpsertPlan> PlanAsync(IProfileAccountApi api, string access, LocalProfileStore store, string game,
        Func<string, bool> saveFolderExists, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(api); ArgumentNullException.ThrowIfNull(store);
        var me = await api.MeAsync(access, cancellation).ConfigureAwait(false);
        var localOnly = store.Read().Where(p => p.Game == game && p.LocalOnly).Select(p => p.Id).ToHashSet(StringComparer.Ordinal);
        var all = store.RefreshFromAccount(me, game, saveFolderExists);
        var account = all.Where(p => p.Game == game && p.OnAccount(me.AccountId)).ToArray();
        var uploadable = all.Where(p => p.Game == game && p.LocalOnly).ToArray();
        var room = Math.Max(0, LocalProfileStore.MaxProfiles - me.Profiles.Count(p => p.Game == game));
        return new(me.AccountId, account, uploadable, room) { Recovered = account.Where(p => localOnly.Contains(p.Id)).ToArray() };
    }

    /// <summary>What <see cref="ContinueOfflineSave"/> did with a registered profile's local save.</summary>
    public enum SaveContinuation
    {
        /// <summary>Nothing to do: Hardcore (online starts from the server's seed, OD-60; its local progress stays offline), or no local save.</summary>
        None,

        /// <summary>The newest sealed local revision now continues in the account XUID's folder (P1b, OD-41 clone with backups).</summary>
        Continued,

        /// <summary>The clone failed; the local save is untouched and stays on this PC.</summary>
        Kept,
    }

    /// <summary>
    /// P1b, after a registration: a Noob profile's newest sealed revision in its own offline folder is cloned into the account XUID's
    /// folder (what that folder held is backed up first) and recorded in <see cref="SaveLinks"/>, so its offline progress continues
    /// online. A Hardcore profile's local save is never cloned or uploaded (OD-60, OD-120).
    /// </summary>
    public static SaveContinuation ContinueOfflineSave(string savesRoot, string stateDirectory, LocalProfile profile, DateTime nowUtc)
    {
        ArgumentNullException.ThrowIfNull(profile);
        if (profile.Hardcore || profile.Cloud is not { } cloud
            || !ulong.TryParse(cloud.Xuid, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var xuid)) return SaveContinuation.None;
        try
        {
            var source = AzimuthSaves.List(savesRoot).FirstOrDefault(s => s.Xuid == profile.OfflineXuid);
            if (source is null) return SaveContinuation.None;
            var result = AzimuthSaves.Clone(savesRoot, source, xuid, nowUtc);
            new SaveLinks(stateDirectory).Set(cloud.Xuid, new SaveLink(source.Folder, source.Revision.Number, result.Number, nowUtc));
            return SaveContinuation.Continued;
        }
        catch (Exception error) when (error is SaveException or IOException or UnauthorizedAccessException) { return SaveContinuation.Kept; }
    }

    /// <summary>
    /// Uploads <paramref name="chosen"/> (at most the plan's room), one at a time; each refusal is reported with its code and the
    /// next one still goes. A transport failure stops the batch and is thrown: the stored keys make the retry safe.
    /// </summary>
    public static async Task<IReadOnlyList<ProfileUpsertResult>> UploadAsync(IProfileAccountApi api, string access, LocalProfileStore store,
        ProfileUpsertPlan plan, IReadOnlyCollection<string> chosen, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(api); ArgumentNullException.ThrowIfNull(store); ArgumentNullException.ThrowIfNull(plan); ArgumentNullException.ThrowIfNull(chosen);
        var picked = plan.Uploadable.Where(p => chosen.Contains(p.Id)).ToArray();
        if (picked.Length > plan.Room) throw new LocalProfileException(ErrorCodes.AccountsProfileLimit);
        var results = new List<ProfileUpsertResult>();
        foreach (var local in picked)
        {
            // The stored copy decides (another window may have uploaded it meanwhile).
            var current = store.Read().FirstOrDefault(p => p.Id == local.Id);
            if (current is null || !current.LocalOnly) { if (current is not null) results.Add(new(current, null)); continue; }
            try
            {
                var created = await api.CreateProfileAsync(access, new NewProfile(current.Game, current.Name, current.Mode), current.UploadKey, cancellation).ConfigureAwait(false);
                results.Add(new(store.Link(current.Id, new CloudLink(plan.AccountId, created.ProfileId, created.Xuid, created.Gamertag)), null));
            }
            catch (ApiException refused) when (refused.Code is ErrorCodes.AccountsProfileLimit or ErrorCodes.AccountsNameUnavailable
                or ErrorCodes.AccountsInvalidName or ErrorCodes.AccountsRegistrationDisabled or ErrorCodes.RequestIdempotencyKeyReused or ErrorCodes.AccountsFrozen)
            {
                results.Add(new(current, refused.Code));
            }
        }
        return results.AsReadOnly();
    }

    /// <summary>
    /// OD-120 free rename of an account profile (<c>POST /profiles/{xuid}/name</c>, allowed while it has a <c>renameNotice</c>): its
    /// current name keeps it, another free name takes it. The answer is stored at once (name, notice cleared). A refusal comes back as
    /// its code (<c>NAME_UNAVAILABLE</c>, <c>INVALID_NAME</c>, <c>RENAME_NOT_ALLOWED</c>, <c>PROFILE_IN_USE</c>, ...); a transport
    /// failure is thrown, and <paramref name="idempotencyKey"/> makes its retry replay the first outcome.
    /// </summary>
    public static async Task<ProfileUpsertResult> RenameAsync(IProfileAccountApi api, string access, LocalProfileStore store, LocalProfile profile,
        string name, string idempotencyKey, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(api); ArgumentNullException.ThrowIfNull(store); ArgumentNullException.ThrowIfNull(profile);
        if (profile.Cloud is not { } cloud) throw new InvalidOperationException("only an account profile is renamed on the account");
        if (!ProfileName.IsValid(name)) return new(profile, ErrorCodes.AccountsInvalidName);
        try
        {
            var renamed = await api.RenameProfileAsync(access, cloud.Xuid, new NameChange(name), idempotencyKey, cancellation).ConfigureAwait(false);
            return new(store.ApplyAccountProfile(cloud.AccountId, renamed) ?? profile, null);
        }
        catch (ApiException refused) when (refused.Code.StartsWith("ACCOUNTS.", StringComparison.Ordinal)
            || refused.Code is ErrorCodes.SessionsProfileInUse or ErrorCodes.RequestIdempotencyKeyReused)
        {
            return new(profile, refused.Code);
        }
    }
}
