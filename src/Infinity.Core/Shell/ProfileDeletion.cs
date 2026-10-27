// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Saves;

namespace Infinity.Core.Shell;

/// <summary>How a <c>DELETE /launcher/v1/profiles/{xuid}</c> ended (CR-S61, ACCOUNTS.md §5 "Profile deletion").</summary>
public enum ProfileDeletionResult
{
    /// <summary>204, a replay's 204, or 404 <c>ACCOUNTS.NOT_FOUND</c> (already deleted, e.g. after a lost 204): the profile is gone.</summary>
    Deleted,
    /// <summary>429 <c>REQUEST.RATE_LIMITED</c>: 3 deletions per account per rolling hour; <see cref="ProfileDeletionOutcome.RetryAfter"/>.</summary>
    RateLimited,
    /// <summary>409 <c>SESSIONS.PROFILE_IN_USE</c>: a session, a live lease or a game ticket holds the profile.</summary>
    InUse,
    /// <summary>403 <c>ACCOUNTS.DELETION_PENDING</c>: the account (or profile) is pending deletion.</summary>
    DeletionPending,
    /// <summary>403 <c>ACCOUNTS.BANNED</c> (a review block included, <c>details.review</c>).</summary>
    Banned,
}

/// <param name="RetryAfter">When to try again: <c>Retry-After</c>, or for a ticket <c>details.ticketExpiresAt</c> minus now.</param>
/// <param name="Details">The refusal's <c>details</c> (the ban's <c>review</c>).</param>
public sealed record ProfileDeletionOutcome(ProfileDeletionResult Result, TimeSpan? RetryAfter = null, JsonElement? Details = null)
{
    public bool Done => Result == ProfileDeletionResult.Deleted;

    /// <summary>Whole minutes to wait, at least 1; null when the server gave no time.</summary>
    public int? RetryMinutes => RetryAfter is { } wait && wait > TimeSpan.Zero ? Math.Max(1, (int)Math.Ceiling(wait.TotalMinutes)) : null;
}

/// <summary>What the PC kept after a profile was deleted from the cloud.</summary>
/// <param name="Archived">The save folders moved to <c>&lt;saves&gt;\fi-launcher-archive\User_x\cloud-deleted-&lt;stamp&gt;</c>.</param>
/// <param name="NotArchived">Folders that could not be moved now (in use, no rights): left where they were, never deleted.</param>
/// <param name="Dropped">The local entry removed from <c>local-profiles.json</c>, if this launcher held one.</param>
/// <param name="SyncForgotten">The profile's sync state on this server was removed.</param>
public sealed record ProfileDeletionCleanup(IReadOnlyList<string> Archived, IReadOnlyList<string> NotArchived, LocalProfile? Dropped, bool SyncForgotten)
{
    public bool FullyArchived => NotArchived.Count == 0;
}

/// <summary>
/// CR-S61 (owner 2026-10-27 "Players delete their own profiles from the cloud, at once, 3 an hour"): the request, its outcome, and what
/// the launcher does on this PC afterwards. The cloud data goes for good; the PC's save is archived, never deleted (the save rules).
/// </summary>
public static class ProfileDeletion
{
    public const string ArchiveLabel = "cloud-deleted";

    /// <summary>
    /// Runs <paramref name="delete"/> (the DELETE under the caller's Idempotency-Key, kept for retries of this same deletion); a ticket's
    /// expiry is compared with <paramref name="now"/> (the server's clock). The contract's refusals are outcomes; anything else (a transport failure, 5xx, <c>REQUEST.INVALID</c>, a closed service) throws.
    /// </summary>
    public static async Task<ProfileDeletionOutcome> SendAsync(Func<CancellationToken, Task> delete, Func<DateTimeOffset> now, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(delete);
        ArgumentNullException.ThrowIfNull(now);
        try
        {
            await delete(cancellation).ConfigureAwait(false);
            return new ProfileDeletionOutcome(ProfileDeletionResult.Deleted);
        }
        catch (ApiException refused) when (Classify(refused, now()) is { } outcome)
        {
            return outcome;
        }
    }

    /// <summary>The contract's answer as an outcome; null for an answer the caller handles as any error.</summary>
    public static ProfileDeletionOutcome? Classify(ApiException error, DateTimeOffset now)
    {
        ArgumentNullException.ThrowIfNull(error);
        return error.Code switch
        {
            ErrorCodes.AccountsNotFound when error.Status == HttpStatusCode.NotFound => new(ProfileDeletionResult.Deleted),
            ErrorCodes.RequestRateLimited => new(ProfileDeletionResult.RateLimited, error.RetryAfter, error.Details),
            ErrorCodes.SessionsProfileInUse => new(ProfileDeletionResult.InUse, error.RetryAfter ?? TicketWait(error.Details, now), error.Details),
            ErrorCodes.AccountsDeletionPending => new(ProfileDeletionResult.DeletionPending, null, error.Details),
            ErrorCodes.AccountsBanned => new(ProfileDeletionResult.Banned, null, error.Details),
            _ => null,
        };
    }

    /// <summary><c>details.ticketExpiresAt</c> minus <paramref name="now"/>, when it is a future RFC 3339 time.</summary>
    private static TimeSpan? TicketWait(JsonElement? details, DateTimeOffset now) =>
        details is { ValueKind: JsonValueKind.Object } d && d.TryGetProperty("ticketExpiresAt", out var at) && at.ValueKind == JsonValueKind.String
        && DateTimeOffset.TryParse(at.GetString(), CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal, out var expires) && expires > now
            ? expires - now : null;

    /// <summary>
    /// After <see cref="ProfileDeletionResult.Deleted"/>: the profile's save folders on this PC (its account XUID's
    /// <c>player-1\User_&lt;hex&gt;</c> and, when this launcher held a local entry, that entry's own offline folder) move whole to
    /// <c>fi-launcher-archive\User_&lt;hex&gt;\cloud-deleted-&lt;stamp&gt;</c> on the same volume; the local entry leaves
    /// <c>local-profiles.json</c>; <paramref name="forgetSync"/> removes the sync marks. Nothing is deleted; a folder in use stays put.
    /// </summary>
    public static ProfileDeletionCleanup CleanUp(string? savesRoot, LocalProfileStore store, string accountId, Profile profile,
        Func<ulong, bool>? forgetSync, DateTime nowUtc)
    {
        ArgumentNullException.ThrowIfNull(store);
        ArgumentNullException.ThrowIfNull(profile);
        var xuids = new List<ulong>();
        if (ulong.TryParse(profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var accountXuid)) xuids.Add(accountXuid);
        var local = store.Read().FirstOrDefault(p => p.Cloud is { } cloud && cloud.AccountId == accountId && cloud.ProfileId == profile.ProfileId);
        if (local is not null && !xuids.Contains(local.OfflineXuid)) xuids.Add(local.OfflineXuid);
        var archived = new List<string>();
        var kept = new List<string>();
        if (savesRoot is not null)
        {
            foreach (var xuid in xuids)
            {
                var folder = CloudSaveFiles.ProfileDir(savesRoot, xuid);
                if (!Directory.Exists(folder)) continue;
                try
                {
                    if (AzimuthSaves.IsBusyFolder(folder)) { kept.Add(folder); continue; }
                    archived.Add(Archive(savesRoot, xuid, folder, nowUtc));
                }
                catch (Exception e) when (e is IOException or UnauthorizedAccessException) { kept.Add(folder); }
            }
        }
        LocalProfile? dropped = null;
        try { dropped = store.ForgetAccountProfile(accountId, profile.ProfileId); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { } // the next /me marks it not on the account
        var forgotten = false;
        if (forgetSync is not null && accountXuid != 0)
        {
            try { forgotten = forgetSync(accountXuid); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
        }
        return new ProfileDeletionCleanup(archived, kept, dropped, forgotten);
    }

    private static string Archive(string savesRoot, ulong xuid, string folder, DateTime nowUtc)
    {
        var archive = Path.Combine(savesRoot, AzimuthSaves.ArchiveDir, AzimuthSaves.FolderFor(xuid));
        Directory.CreateDirectory(archive);
        var stamp = nowUtc.ToString("yyyyMMdd'T'HHmmss'Z'", CultureInfo.InvariantCulture);
        var target = Path.Combine(archive, $"{ArchiveLabel}-{stamp}");
        for (var n = 1; Directory.Exists(target); n++) target = Path.Combine(archive, $"{ArchiveLabel}-{stamp}-{n}");
        Directory.Move(folder, target);
        return target;
    }
}
