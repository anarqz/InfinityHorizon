// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Text.Json;
using Infinity.Core.Api;

namespace Infinity.Core.Saves;

/// <summary>What the dry run of a seed came to, in the terms the creation screen shows (SAVES_API.md §4.10, §6).</summary>
public enum SeedCheckOutcome
{
    /// <summary>200, <c>accepted: true</c>: a <c>seed_from_profile</c> commit of these bytes would be taken now.</summary>
    Accepted,

    /// <summary>200, <c>accepted: false</c>: the check refused it, with its reasons (an unreadable bundle is <c>BUNDLE_INVALID</c>).</summary>
    Refused,

    /// <summary>429 <c>SAVES.RATE_LIMITED</c>: 5 checks per account per day; <c>Retry-After</c> says when.</summary>
    RateLimited,

    /// <summary>409 <c>SAVES.HARDCORE_NEEDS_FRESH_CAREER {reason: "source_used"}</c>: that profile already seeded a live line.</summary>
    SourceUsed,

    /// <summary>The server does not offer it for this profile now: the option off or a <c>seed</c> line (409), no cloud line (403), revisions already (412).</summary>
    Unavailable,

    /// <summary>403 <c>ACCOUNTS.BANNED {review: "seed_from_profile"}</c>: the account is blocked pending a moderator's review.</summary>
    UnderReview,

    /// <summary>Any other answer (a client bug, the network, the server down): nothing was decided.</summary>
    Failed,
}

/// <summary>A dry run's answer: the check on 200, else the code and <c>Retry-After</c>.</summary>
public sealed record SeedCheckResult(SeedCheckOutcome Outcome, SeedCheck? Check, string? Code, TimeSpan? RetryAfter);

/// <summary>
/// CR-S52 (owner 2026-10-26, "a Hardcore profile seeded from an existing profile, checked on the spot"): a new Hardcore profile may
/// start from another of the account's profiles' career, where the server allows it. The order is the contract's: the Hardcore profile
/// is created on the server first, then the source's newest sealed local revision is checked (<c>POST .../save/check</c>), the result is
/// shown, and only after the player confirms an accepted check is that revision copied into the new profile's folder and recorded as the
/// pending <c>seed_from_profile</c> commit, sent at the profile's first launch before the game starts (<see cref="SessionSaves"/>). Any
/// refusal leaves the profile on its <c>fresh_start</c> (an empty career, CR-S51). The source's own folder is only read.
/// </summary>
public static class ProfileSeed
{
    /// <summary><c>ACCOUNTS.BANNED</c> <c>details.review</c> of a seeded line whose source was found altered (ACCOUNTS.md §10).</summary>
    public const string BanReview = "seed_from_profile";

    /// <summary><c>SAVES.HARDCORE_NEEDS_FRESH_CAREER</c> <c>details.reason</c>: the source already seeded a live line.</summary>
    public const string SourceUsedReason = "source_used";

    /// <summary>The <c>SAVES.GATE_REFUSED</c> rule of a refused <c>seed_from_profile</c> commit.</summary>
    public const string NotPlausibleSeed = "NOT_PLAUSIBLE_SEED";

    /// <summary>The archive label of what the seed's install shadows in the new profile's folder.</summary>
    public const string ArchiveLabel = "seed-from-profile";

    /// <summary>
    /// The one condition for offering "Start from one of my profiles" on the Hardcore creation: the server allows it
    /// (<c>/info saves.hardcoreFromProfile</c>) and the new Hardcore profile keeps a cloud line here (CR-S51 flags), since the seed is a
    /// commit at revision 0 of that line. Kept in one place: whether a Hardcore profile saved only on this PC may use it is the server
    /// owner's to settle.
    /// </summary>
    public static bool Offered(bool serverAllows, bool hardcoreKeepsCloud) => serverAllows && hardcoreKeepsCloud;

    /// <summary>
    /// The profiles a Hardcore profile may start from (§4.10 "the source"): the account's own live profiles on the server, of the same
    /// game, in any mode, the target excluded. The caller passes the account's profiles only (<c>/me</c>): a profile kept only on this PC
    /// is registered first, and Azimuth's own career without a profile is never a source.
    /// </summary>
    public static IReadOnlyList<Profile> Sources(IEnumerable<Profile> profiles, string game, string? targetXuid = null)
    {
        ArgumentNullException.ThrowIfNull(profiles);
        return [.. profiles.Where(p => p.Game == game && p.Status == "active" && p.Xuid != targetXuid
            && ulong.TryParse(p.Xuid, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out _))];
    }

    /// <summary>
    /// The source's newest sealed revision on this PC (<c>User_&lt;source XUID&gt;</c>, the folder an account's Noob profile plays),
    /// read and packed: the bytes the dry run checks and, on confirm, the ones copied and committed. Null when this PC has none.
    /// </summary>
    public static PackedRevision? ReadSource(string savesRoot, ulong sourceXuid)
    {
        var newest = AzimuthSaves.NewestSealed(CloudSaveFiles.ProfileDir(savesRoot, sourceXuid));
        return newest is null ? null : CloudSaveFiles.Pack(newest);
    }

    /// <summary>
    /// The confirmed seed: <paramref name="packed"/>'s files installed as the newest revision of the new profile's folder (what it
    /// shadows archived first; the source's folder untouched) and recorded as its pending <c>seed_from_profile</c> upload. Only while the
    /// game is not running (the caller checks). Null when done.
    /// </summary>
    public static SaveFailure? Adopt(CloudSaves saves, string savesRoot, ulong targetXuid, ulong sourceXuid, PackedRevision packed)
    {
        ArgumentNullException.ThrowIfNull(saves);
        ArgumentNullException.ThrowIfNull(packed);
        if (targetXuid == sourceXuid) throw new ArgumentException("a profile cannot seed itself", nameof(sourceXuid));
        if (saves.State(targetXuid)?.PendingUpload is not null)
        {
            return SaveFailure.Local(SaveFailureKind.UploadPending, "another upload of the new profile is pending");
        }
        InstalledRevision installed;
        try
        {
            installed = CloudSaveFiles.Install(savesRoot, targetXuid, packed.Files, packed.ContentSha256, Guid.CreateVersion7().ToString("N"), ArchiveLabel);
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException)
        {
            return SaveFailure.Integrity(e.Message);
        }
        return saves.RecordSeed(targetXuid, packed, installed.Number, sourceXuid);
    }

    /// <summary>
    /// The first Play of an existing Hardcore profile offers the choice (owner/server follow-up 2026-10-27) when its line is still empty
    /// and starts fresh (<c>revision 0</c>, <c>start: fresh_start</c>) and no upload of it is pending on this PC (a recorded first career
    /// or an adopted seed goes first instead). "Start from scratch" is then §4.9 as before: the folder archived, the game on an empty one.
    /// </summary>
    public static bool OffersAtFirstLaunch(SaveHead head, PendingUpload? pending)
    {
        ArgumentNullException.ThrowIfNull(head);
        return head.Revision == 0 && head.Start == SaveStarts.FreshStart && pending is null;
    }

    /// <summary>Seed checks per account per day, dry runs and <c>seed_from_profile</c> commits together (§4.10).</summary>
    public const int DailyChecks = 5;

    /// <summary>
    /// After an accepted dry run: the first upload is one more check, and this launcher counted <paramref name="checksThisDay"/> (the
    /// server does not say how many are left). True when the commit may meet 429 (the warning on the accepted screen).
    /// </summary>
    public static bool CommitMayBeLimited(int checksThisDay) => checksThisDay >= DailyChecks - 1;

    /// <summary>
    /// "Start a new career" after this answer records the player's choice (the first Play stops asking): only an answer that says
    /// something about the career or its source (accepted, refused, source used). An answer that decided nothing (the day's checks
    /// used, the server unavailable, a failure) never does: the player may try again later.
    /// </summary>
    public static bool DeclinesOnStartFresh(SeedCheckOutcome outcome) =>
        outcome is SeedCheckOutcome.Accepted or SeedCheckOutcome.Refused or SeedCheckOutcome.SourceUsed;

    /// <summary>The answer screen offers "Not now" (closes, nothing recorded, no Play) beside "Start a new career".</summary>
    public static bool OffersNotNow(SeedCheckOutcome outcome) => !DeclinesOnStartFresh(outcome);

    /// <summary>
    /// The player gave up a recorded seed that cannot go yet (429 at the first upload): its bytes move to <c>pending\set-aside</c>
    /// (kept), and the next Play starts the profile fresh (§4.9: the folder archived). True when a seed was pending.
    /// </summary>
    public static bool Abandon(CloudSaves saves, ulong targetXuid)
    {
        ArgumentNullException.ThrowIfNull(saves);
        if (saves.State(targetXuid)?.PendingUpload is not { Kind: CommitKinds.SeedFromProfile }) return false;
        saves.SetAsidePending(targetXuid);
        return true;
    }

    /// <summary>A local seed that was not sent (its recorded bytes missing or changed): nothing reached the server.</summary>
    public const string NotSent = "INTEGRITY";

    /// <summary>A 2xx for another revision or content than the seed sent: the head decides what the line holds.</summary>
    public const string Unconfirmed = "LOCAL.SEED_UNCONFIRMED";

    /// <summary>The <see cref="SaveStep.SeedRefused"/> detail of a seed commit that did not become revision 1.</summary>
    public static string? CommitDetail(CommitOutcome sent)
    {
        ArgumentNullException.ThrowIfNull(sent);
        return sent switch
        {
            { Committed: not null, Failure: not null } => Unconfirmed,
            { Failure: { Code: ErrorCodes.AccountsBanned, Review: { } review } } => ErrorCodes.AccountsBanned + "/" + review,
            { Failure: { } f } => f.Rule ?? f.Code,
            _ => null,
        };
    }

    /// <summary>The catalog key telling the player what <see cref="CommitDetail"/> means.</summary>
    public static string CommitTextKey(string? detail) => detail switch
    {
        NotSent => "saves.seed_not_sent",
        Unconfirmed => "saves.seed_unconfirmed",
        ErrorCodes.AccountsBanned + "/" + BanReview => "accounts.banned_seed_review",
        ErrorCodes.AccountsBanned => "native.banned",
        _ when detail?.StartsWith(ErrorCodes.AccountsBanned + "/", StringComparison.Ordinal) == true => "native.banned",
        _ => "saves.seed_refused",
    };

    /// <summary>Maps the dry run's answer (SAVES_API.md §4.10, §6; ACCOUNTS.md §10).</summary>
    public static SeedCheckResult Interpret(SaveApiResult<SeedCheck> answer)
    {
        ArgumentNullException.ThrowIfNull(answer);
        if (answer is { Ok: true, Value: { } check })
        {
            return new SeedCheckResult(check.Accepted ? SeedCheckOutcome.Accepted : SeedCheckOutcome.Refused, check, null, null);
        }
        var error = answer.Error ?? new ErrorBody("HTTP.EMPTY", "empty body", null);
        var outcome = error.Code switch
        {
            _ when (int)answer.Status is >= 200 and <= 299 => SeedCheckOutcome.Failed,
            ErrorCodes.SavesRateLimited => SeedCheckOutcome.RateLimited,
            ErrorCodes.SavesHardcoreNeedsFreshCareer when Detail(error.Details, "reason") == SourceUsedReason => SeedCheckOutcome.SourceUsed,
            ErrorCodes.SavesHardcoreNeedsFreshCareer or ErrorCodes.SavesNotEligible or ErrorCodes.SavesRevisionConflict => SeedCheckOutcome.Unavailable,
            ErrorCodes.AccountsBanned when Detail(error.Details, "review") == BanReview => SeedCheckOutcome.UnderReview,
            _ when answer.Status == HttpStatusCode.TooManyRequests => SeedCheckOutcome.RateLimited,
            _ => SeedCheckOutcome.Failed,
        };
        return new SeedCheckResult(outcome, null, error.Code, answer.RetryAfter);
    }

    /// <summary><c>ACCOUNTS.BANNED</c> pending a review of a seeded line (<c>details.review: "seed_from_profile"</c>).</summary>
    public static bool IsSeedReview(string code, JsonElement? details) => code == ErrorCodes.AccountsBanned && Detail(details, "review") == BanReview;

    /// <summary>A string field of an error's <c>details</c>, or null.</summary>
    public static string? Detail(JsonElement? details, string name) =>
        details is { ValueKind: JsonValueKind.Object } o && o.TryGetProperty(name, out var v) && v.ValueKind == JsonValueKind.String ? v.GetString() : null;

    /// <summary>The reason codes with a text of their own (<c>seed.reason.&lt;CODE&gt;</c>, SAVES_API.md §4.10).</summary>
    public static IReadOnlyList<string> NamedReasons { get; } =
    [
        "BUNDLE_INVALID", "SUMMARY_UNREADABLE", "LIMITS", "PLAYERDB_UNREADABLE", "FIELD_MISSING", "VOUCHER_UNGRANTED",
        "LEVEL_XP_MISMATCH", "TOKENS_SKILL_MISMATCH",
    ];

    /// <summary>
    /// The catalog key of a refusal reason: its own text for the named ones, the career pace (<c>CAREER_*</c>), a ceiling
    /// (<c>*_CEILING</c>), else the generic "unusual progress" text (with the code). Stable codes, so a newer server's code still reads.
    /// </summary>
    public static string ReasonKey(string code)
    {
        ArgumentNullException.ThrowIfNull(code);
        if (NamedReasons.Contains(code, StringComparer.Ordinal)) return "seed.reason." + code;
        if (code.StartsWith("CAREER_", StringComparison.Ordinal)) return "seed.reason.career_pace";
        if (code.EndsWith("_CEILING", StringComparison.Ordinal)) return "seed.reason.ceiling";
        return "seed.reason.other";
    }
}
