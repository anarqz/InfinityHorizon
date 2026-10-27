// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.Net.Http;
using Infinity.Core.Api;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>One profile a Hardcore profile may start from, as the screens list it.</summary>
public sealed record SeedSourceRow(Profile Profile)
{
    public string Name => ProfileName.Plain(Profile.Gamertag) + " · " + (Profile.Mode == "hardcore" ? "HARDCORE" : "NOOB");

    /// <summary>The app's ComboBox template shows the item's text, not <c>DisplayMemberPath</c>.</summary>
    public override string ToString() => Name;
}

/// <summary>A dry run shown to the player: the Hardcore profile, the source, the checked bytes, the server's answer, and whether a Play waits on it.</summary>
internal sealed record SeedOffer(Profile Target, Profile Source, PackedRevision Packed, SeedCheckResult Result, bool FromPlay);

/// <summary>
/// CR-S52 (owner 2026-10-26, "a Hardcore profile seeded from an existing profile, checked on the spot"): on a server that allows it, a
/// Hardcore profile whose cloud line is still empty may start from one of the account's other profiles. The order is the contract's
/// (SAVES_API.md §4.10):
/// <list type="number">
/// <item>the owner's warning shows with the option (the creation form, or the first Play of a Hardcore profile at revision 0), with the
/// limit of 5 checks a day; the player picks the source among the account's own profiles on the server;</item>
/// <item>the Hardcore profile exists on the server (at creation the usual upsert runs first);</item>
/// <item>the source's newest sealed revision on this PC goes to <c>POST .../save/check</c> (the dry run: nothing committed);</item>
/// <item>the answer is shown (accepted, or the reasons in words) and the player confirms or starts a new career;</item>
/// <item>on confirm, that revision is copied into the profile's folder and recorded as the pending <c>seed_from_profile</c> commit, which
/// the next Play sends under its lease before the game starts; a refusal there falls back to <c>fresh_start</c> (§4.9).</item>
/// </list>
/// Nothing is ever copied into a Hardcore profile without an accepted check.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _hardcoreFromProfile;
    private bool _newSeedFromProfile;
    private SeedSourceRow? _newSeedSource;
    private SeedOffer? _seedOffer;
    private Profile? _seedChoice;
    private SeedSourceRow? _seedChoiceSource;
    private readonly HashSet<string> _seedDeclined = new(StringComparer.Ordinal);
    private readonly HashSet<string> _seedSkipOnce = new(StringComparer.Ordinal);
    private readonly List<DateTimeOffset> _seedChecks = [];
    private (Profile Profile, TimeSpan? RetryAfter)? _seedBlocked;

    /// <summary>The profiles whose seed the player declined this launcher run (the harness check reads it).</summary>
    internal IReadOnlySet<string> SeedDeclined => _seedDeclined;

    /// <summary><c>/info saves.hardcoreFromProfile</c> of the signed-in server (absent: false).</summary>
    private bool ServerAllowsSeedFromProfile => _session?.Info.Saves?.HardcoreFromProfile ?? _hardcoreFromProfile;

    /// <summary>The offer's condition (<see cref="ProfileSeed.Offered"/>) for a new Hardcore profile: the server allows it and it keeps a cloud line.</summary>
    public bool CanSeedFromProfile => CanChooseHardcore && ProfileSeed.Offered(ServerAllowsSeedFromProfile, HardcoreKeepsCloud);

    /// <summary>The account's own profiles of the chosen game on the server (never one kept only on this PC), other than <paramref name="targetXuid"/>.</summary>
    private IReadOnlyList<SeedSourceRow> SourcesFor(string? targetXuid) =>
        [.. ProfileSeed.Sources(_session?.Me?.Profiles ?? Profiles.Where(p => !_rowLocals.ContainsKey(p.ProfileId)), _gate.Game ?? "", targetXuid)
            .Select(p => new SeedSourceRow(p))];

    /// <summary>The creation form's sources (the new profile does not exist yet).</summary>
    public IReadOnlyList<SeedSourceRow> SeedSources => SourcesFor(null);

    /// <summary>The form's "Start from one of my profiles" (Hardcore chosen, the offer's condition, a source to pick).</summary>
    public bool ShowSeedOption => NewHardcore && CanSeedFromProfile && SeedSources.Count > 0;

    public bool NewSeedFromProfile
    {
        get => _newSeedFromProfile && ShowSeedOption;
        set
        {
            _newSeedFromProfile = value;
            if (value && _newSeedSource is null && SeedSources is [var only]) _newSeedSource = only;
            ChangedSeedForm();
        }
    }

    public SeedSourceRow? NewSeedSource
    {
        get => _newSeedSource is { } chosen && SeedSources.Contains(chosen) ? chosen : null;
        set { _newSeedSource = value; ChangedSeedForm(); }
    }

    private void ChangedSeedForm()
    {
        Changed(nameof(ShowSeedOption));
        Changed(nameof(NewSeedFromProfile));
        Changed(nameof(NewSeedSource));
        Changed(nameof(SeedSources));
    }

    // ------------------------------------------------------------------ the first Play of a Hardcore profile whose line is empty

    /// <summary>The seed screens cover the shell (the first Play's choice, then the dry run's answer).</summary>
    public bool ShowSeedPanel => _seedChoice is not null || _seedOffer is not null || _seedBlocked is not null;

    /// <summary>The first Play's choice: "Start from scratch" or a source to check.</summary>
    public bool ShowSeedChoice => _seedChoice is not null && _seedOffer is null && _seedBlocked is null;

    public string SeedChoiceTitle => _seedChoice is not { } target ? string.Empty : S.Format("seed.first_launch_title", ("name", ProfileName.Plain(target.Gamertag)));

    public IReadOnlyList<SeedSourceRow> SeedChoiceSources => _seedChoice is { } target ? SourcesFor(target.Xuid) : [];

    public SeedSourceRow? SeedChoiceSource
    {
        get => _seedChoiceSource is { } chosen && SeedChoiceSources.Contains(chosen) ? chosen : null;
        set { _seedChoiceSource = value; Changed(nameof(SeedChoiceSource)); Changed(nameof(CanCheckSeedChoice)); }
    }

    public bool CanCheckSeedChoice => SeedChoiceSource is not null && Idle;

    /// <summary>
    /// Before a Play of <paramref name="profile"/>: true when the choice opened instead (the Play stops; the choice resumes it). Asked once
    /// per launcher run and profile; any doubt (no head, the network) plays as before.
    /// </summary>
    private async Task<bool> OfferSeedBeforePlayAsync(Profile profile)
    {
        if (_seedSkipOnce.Remove(profile.ProfileId)) return false; // "Start a new career" after an answer that decided nothing: this Play only
        if (profile.Mode != "hardcore" || _seedDeclined.Contains(profile.ProfileId) || _session is not { } session
            || !ProfileSeed.Offered(ServerAllowsSeedFromProfile, profile.CloudLine ?? HardcoreKeepsCloud)
            || SourcesFor(profile.Xuid).Count == 0 || CloudSaveClient() is not { } client
            || !ulong.TryParse(profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid))
        {
            return false;
        }
        try
        {
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            var (head, failure) = await client.Saves.HeadAsync(access, xuid);
            if (head is null || failure is not null || !ProfileSeed.OffersAtFirstLaunch(head, client.Saves.State(xuid)?.PendingUpload)) return false;
        }
        catch (Exception e) when (e is HttpRequestException or ApiException or IOException or OperationCanceledException)
        {
            Log("seed: the line was not read before Play: " + e.GetType().Name);
            return false;
        }
        Log("seed: the Hardcore line is empty and starts fresh: the player chooses how it starts");
        _seedChoice = profile;
        _seedChoiceSource = SeedChoiceSources is [var only] ? only : null;
        ChangedSeedCheck();
        return true;
    }

    /// <summary>"Start from scratch": §4.9 as before (the first Play archives the profile's folder on this PC and the game starts a new career).</summary>
    public async Task StartFromScratchAsync()
    {
        if (_seedChoice is not { } target) return;
        _seedDeclined.Add(target.ProfileId);
        _seedChoice = null;
        ChangedSeedCheck();
        await PlayAsync();
    }

    /// <summary>"Not now": the seed screens close without playing and nothing is recorded (asked again at the next Play).</summary>
    public void CloseSeedChoice()
    {
        _seedChoice = null;
        _seedOffer = null;
        _seedBlocked = null;
        ChangedSeedCheck();
    }

    // ------------------------------------------------------------------ a seed whose first upload waits on the day's checks (429)

    public bool ShowSeedBlocked => _seedBlocked is not null;

    public string SeedBlockedText => _seedBlocked is not { } blocked ? string.Empty : blocked.RetryAfter is { } wait
        ? S.Format("seed.blocked_wait", ("name", ProfileName.Plain(blocked.Profile.Gamertag)), ("hours", Infinity.Core.Overlay.OverlayNotices.Hours(wait).ToString(CultureInfo.CurrentCulture)))
        : S.Format("seed.blocked", ("name", ProfileName.Plain(blocked.Profile.Gamertag)));

    /// <summary>A Play stopped on <c>SAVES.RATE_LIMITED</c> while the chosen profile's seed was pending: offer a new career instead.</summary>
    private void ObserveSaveBlocked(string detail)
    {
        var (code, retryAfter) = Infinity.Core.Overlay.OverlayNotices.ParseBlocked(detail);
        if (code != ErrorCodes.SavesRateLimited || CloudSaveClient() is not { } client
            || _session?.Me?.Profiles.FirstOrDefault(p => p.ProfileId == _gate.ProfileKey && p.Game == _gate.Game) is not { } profile
            || !ulong.TryParse(profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)
            || client.Saves.State(xuid)?.PendingUpload is not { Kind: CommitKinds.SeedFromProfile }) return;
        _seedBlocked = (profile, retryAfter);
        ChangedSeedCheck();
    }

    /// <summary>
    /// "Start a new career instead": the waiting seed is set aside (its bytes kept in <c>pending\set-aside</c>) and this Play starts the
    /// profile fresh (§4.9: its folder archived on this PC by the first Play), so the profile is not blocked until the checks come back.
    /// </summary>
    public async Task AbandonSeedAsync()
    {
        if (_seedBlocked is not { } blocked || CloudSaveClient() is not { } client
            || !ulong.TryParse(blocked.Profile.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)) return;
        _seedBlocked = null;
        ChangedSeedCheck();
        if (ProfileSeed.Abandon(client.Saves, xuid)) Log("seed: the waiting seed was set aside; the profile starts fresh");
        _seedDeclined.Add(blocked.Profile.ProfileId);
        Message = S.Format("seed.fresh", ("name", ProfileName.Plain(blocked.Profile.Gamertag)));
        await PlayAsync();
    }

    /// <summary>The first Play's "Check this career": the dry run for the existing Hardcore profile.</summary>
    public Task CheckSeedChoiceAsync() => _seedChoice is { } target && SeedChoiceSource is { } source
        ? Run(() => CheckSeedAsync(target, source.Profile, fromPlay: true)) : Task.CompletedTask;

    // ------------------------------------------------------------------ the dry run's answer, shown before the player confirms

    public bool ShowSeedCheck => _seedOffer is not null;

    public bool SeedCheckAccepted => _seedOffer?.Result.Outcome == SeedCheckOutcome.Accepted;

    public bool SeedCheckRefused => _seedOffer is not null && !SeedCheckAccepted;

    /// <summary>"Not now" beside "Start a new career" when the answer decided nothing (<see cref="ProfileSeed.OffersNotNow"/>).</summary>
    public bool ShowSeedNotNow => _seedOffer is { } o && ProfileSeed.OffersNotNow(o.Result.Outcome);

    /// <summary>Checks this launcher sent in the last 24 hours (the server does not say how many are left).</summary>
    private int SeedChecksToday
    {
        get { var since = DateTimeOffset.UtcNow.AddHours(-24); _seedChecks.RemoveAll(t => t < since); return _seedChecks.Count; }
    }

    /// <summary>The accepted screen warns that the first upload is one more check when the day's may be used up.</summary>
    public bool SeedCommitMayBeLimited => SeedCheckAccepted && ProfileSeed.CommitMayBeLimited(SeedChecksToday);

    public string SeedCheckTitle => _seedOffer is not { } o ? string.Empty
        : S.Format(SeedCheckAccepted ? "seed.accepted_title" : "seed.refused_title", ("name", ProfileName.Plain(o.Target.Gamertag)), ("source", ProfileName.Plain(o.Source.Gamertag)));

    public string SeedCheckText => _seedOffer is not { } o ? string.Empty : SeedOutcomeText(o.Result, ProfileName.Plain(o.Source.Gamertag));

    /// <summary>The refusal's reasons in words, one line each (one family once).</summary>
    public string SeedCheckReasons => _seedOffer?.Result is not { Outcome: SeedCheckOutcome.Refused, Check: { } check } ? string.Empty
        : string.Join(Environment.NewLine, check.Reasons.Select(SeedReasonText).Distinct(StringComparer.Ordinal).Select(r => "• " + r));

    /// <summary>What the server read of the source: level, credits and the play time it measured over.</summary>
    public string SeedCheckSummary
    {
        get
        {
            if (_seedOffer?.Result.Check is not { } check) return string.Empty;
            var parts = new List<string>();
            if (check.Summary.Level is { } level) parts.Add(S.Format("saves.level", ("level", level.ToString(CultureInfo.CurrentCulture))));
            if (check.Summary.Credits is { } credits) parts.Add(S.Format("saves.credits", ("credits", credits.ToString("N0", CultureInfo.CurrentCulture))));
            parts.Add(S.Format("seed.hours", ("hours", check.Hours.ToString("0.#", CultureInfo.CurrentCulture))));
            return string.Join(" · ", parts);
        }
    }

    internal string SeedReasonText(string code)
    {
        var key = ProfileSeed.ReasonKey(code);
        return key == "seed.reason.other" ? S.Format(key, ("code", code)) : S[key];
    }

    internal string SeedOutcomeText(SeedCheckResult result, string source) => result.Outcome switch
    {
        SeedCheckOutcome.Accepted => S.Format("seed.accepted", ("source", source)),
        SeedCheckOutcome.Refused => S["seed.refused"],
        SeedCheckOutcome.RateLimited => result.RetryAfter is { } wait
            ? S.Format("seed.rate_limited_wait", ("hours", Math.Max(1, (int)Math.Ceiling(wait.TotalHours)).ToString(CultureInfo.CurrentCulture)))
            : S["seed.rate_limited"],
        SeedCheckOutcome.SourceUsed => S["seed.source_used"],
        SeedCheckOutcome.Unavailable => S["seed.unavailable"],
        SeedCheckOutcome.UnderReview => S["accounts.banned_seed_review"],
        _ => (result.Code is { } code ? ErrorText(code) + " " : string.Empty) + S["seed.failed"],
    };

    /// <summary>
    /// Step 3: the dry run for <paramref name="target"/> (a Hardcore profile on the server) from <paramref name="source"/>'s newest sealed
    /// revision on this PC. Whatever the answer, nothing is copied yet.
    /// </summary>
    private async Task CheckSeedAsync(Profile target, Profile source, bool fromPlay)
    {
        var sourceName = ProfileName.Plain(source.Gamertag);
        if (_install is null || _session is not { } session || CloudSaveClient() is not { } client
            || ParseXuid(target) is null || ParseXuid(source) is not { } sourceXuid)
        {
            Message = S["seed.failed"];
            return;
        }
        var savesRoot = AzimuthSaves.SavesRoot(_install.Root);
        PackedRevision? packed;
        try { packed = await Task.Run(() => ProfileSeed.ReadSource(savesRoot, sourceXuid)); }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        {
            Log("seed: the source's save was not read: " + e.GetType().Name);
            packed = null;
        }
        if (packed is null)
        {
            Message = S.Format("seed.no_source_save", ("source", sourceName));
            return;
        }
        SeedCheckResult result;
        try
        {
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            Message = S["seed.checking"];
            var answer = await client.Api.CheckSeedAsync(access, target.Xuid, source.Xuid, packed.Bundle);
            _seedChecks.Add(DateTimeOffset.UtcNow); // the server counts every check it answers, accepted or refused (§4.10)
            if (answer.Status == System.Net.HttpStatusCode.TooManyRequests)
                while (SeedChecksToday < ProfileSeed.DailyChecks) _seedChecks.Add(DateTimeOffset.UtcNow); // the day's are used up
            result = ProfileSeed.Interpret(answer);
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException)
        {
            Log("seed: check unreachable: " + e.GetType().Name);
            Message = S["native.unreachable"] + " " + S["seed.failed"];
            return;
        }
        Log($"seed: check of {packed.ContentSha256[..12]} for the Hardcore profile: {result.Outcome} {result.Code} {string.Join(",", result.Check?.Reasons ?? [])}");
        _seedOffer = new SeedOffer(target, source, packed, result, fromPlay);
        Message = null;
        ChangedSeedCheck();
    }

    /// <summary>Step 5: the player confirmed an accepted check: the checked revision is copied in and recorded; a waiting Play goes on.</summary>
    public async Task ConfirmSeedAsync()
    {
        if (_seedOffer is not { Result.Outcome: SeedCheckOutcome.Accepted } offer) return;
        if (Playing || System.Diagnostics.Process.GetProcessesByName("azimuth").Length > 0) { Message = S["saves.game_running"]; return; }
        if (_install is null || CloudSaveClient() is not { } client || ParseXuid(offer.Target) is not { } targetXuid || ParseXuid(offer.Source) is not { } sourceXuid) return;
        var savesRoot = AzimuthSaves.SavesRoot(_install.Root);
        var adopted = false;
        await Run(async () =>
        {
            var failure = await Task.Run(() => ProfileSeed.Adopt(client.Saves, savesRoot, targetXuid, sourceXuid, offer.Packed));
            _seedOffer = null;
            _seedChoice = null;
            ChangedSeedCheck();
            var (name, source) = (ProfileName.Plain(offer.Target.Gamertag), ProfileName.Plain(offer.Source.Gamertag));
            if (failure is not null)
            {
                Log("seed: not copied: " + failure.Code + " " + failure.Message);
                Message = S.Format("seed.copy_failed", ("name", name));
                return;
            }
            adopted = true;
            Log($"seed: {offer.Packed.ContentSha256[..12]} copied for the next Play (seed_from_profile)");
            Message = S.Format("seed.adopted", ("name", name), ("source", source));
        });
        if (adopted && offer.FromPlay) await PlayAsync(); // the commit goes under this Play's lease, before the game starts
    }

    /// <summary>
    /// "Start a new career": nothing is copied, the profile starts fresh (CR-S51); a waiting Play goes on. Recorded as the player's choice
    /// only after an answer about the career (<see cref="ProfileSeed.DeclinesOnStartFresh"/>); after one that decided nothing (the day's
    /// checks used, the server unavailable, a failure) it holds for this Play only, so the next one asks again.
    /// </summary>
    public async Task DismissSeedAsync()
    {
        if (_seedOffer is not { } offer) return;
        _seedOffer = null;
        _seedChoice = null;
        if (ProfileSeed.DeclinesOnStartFresh(offer.Result.Outcome)) _seedDeclined.Add(offer.Target.ProfileId);
        else if (offer.FromPlay) _seedSkipOnce.Add(offer.Target.ProfileId);
        ChangedSeedCheck();
        Message = S.Format("seed.fresh", ("name", ProfileName.Plain(offer.Target.Gamertag)));
        if (offer.FromPlay) await PlayAsync();
    }

    private void ChangedSeedCheck()
    {
        Changed(nameof(ShowSeedPanel));
        Changed(nameof(ShowSeedChoice));
        Changed(nameof(SeedChoiceTitle));
        Changed(nameof(SeedChoiceSources));
        Changed(nameof(SeedChoiceSource));
        Changed(nameof(CanCheckSeedChoice));
        Changed(nameof(ShowSeedCheck));
        Changed(nameof(SeedCheckAccepted));
        Changed(nameof(SeedCheckRefused));
        Changed(nameof(ShowSeedNotNow));
        Changed(nameof(SeedCommitMayBeLimited));
        Changed(nameof(ShowSeedBlocked));
        Changed(nameof(SeedBlockedText));
        Changed(nameof(SeedCheckTitle));
        Changed(nameof(SeedCheckText));
        Changed(nameof(SeedCheckReasons));
        Changed(nameof(SeedCheckSummary));
    }

    /// <summary>ACCOUNTS.BANNED in words, with the CR-S52 review when <c>details.review</c> says so (ACCOUNTS.md §10).</summary>
    internal string BannedText(System.Text.Json.JsonElement? details) =>
        ProfileSeed.IsSeedReview(ErrorCodes.AccountsBanned, details) ? S["accounts.banned_seed_review"] : S["native.banned"];
}
