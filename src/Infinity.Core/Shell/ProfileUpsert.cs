// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;

namespace Infinity.Core.Shell;

/// <summary>The account calls the update panel needs (the existing API, ACCOUNTS.md; no new route).</summary>
public interface IProfileAccountApi
{
    Task<Me> MeAsync(string access, CancellationToken cancellation);
    Task<Profile> CreateProfileAsync(string access, NewProfile profile, string idempotencyKey, CancellationToken cancellation);
}

/// <summary><see cref="IProfileAccountApi"/> over <see cref="ApiClient"/>.</summary>
public sealed class ApiProfileAccount(ApiClient api) : IProfileAccountApi
{
    public Task<Me> MeAsync(string access, CancellationToken cancellation) => api.MeAsync(access, cancellation);
    public Task<Profile> CreateProfileAsync(string access, NewProfile profile, string idempotencyKey, CancellationToken cancellation) =>
        api.CreateProfileAsync(access, profile, idempotencyKey, cancellation);
}

/// <summary>
/// What the "Update online profiles" panel shows after sign-in: the account's profiles (cached ones refreshed, other PCs' added),
/// the local profiles that may be uploaded, and the room left under the per-account limit. When the local ones do not all fit,
/// the player chooses which to upload; the rest stay offline-only.
/// </summary>
public sealed record ProfileUpsertPlan(string AccountId, IReadOnlyList<LocalProfile> Account, IReadOnlyList<LocalProfile> Uploadable, int Room)
{
    public bool MustChoose => Uploadable.Count > Room;
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
        var all = store.RefreshFromAccount(me, game, saveFolderExists);
        var account = all.Where(p => p.Game == game && p.OnAccount(me.AccountId)).ToArray();
        var uploadable = all.Where(p => p.Game == game && p.LocalOnly).ToArray();
        var room = Math.Max(0, LocalProfileStore.MaxProfiles - me.Profiles.Count(p => p.Game == game));
        return new(me.AccountId, account, uploadable, room);
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
}
