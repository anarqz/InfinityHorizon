// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.Net.Http;
using Infinity.Core.Api;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// OD-214 (CAREERS.md §4, §6.7, §6.18): sign in → pick a game → pick a profile. A profile without a career of the chosen game (or with an
/// empty one) opens the Start panel instead of being chosen: Fresh, or from an existing profile (Hardcore: the account's other profile's
/// career of the same game through the SAVES_API §4.10 check; Noob: a save on this PC, the OD-41 picker). The career is created empty
/// first (<c>POST /players/{xuid}/careers</c>, one key per profile and game until answered; 409 <c>CAREER_EXISTS</c> continues), and the
/// gate is set only once the career exists.
/// </summary>
public sealed partial class ShellViewModel
{
    private ProfileRow? _startRow;
    private readonly Dictionary<(string Xuid, string Game), string> _startKeys = [];

    /// <summary>A Hardcore career started "from an existing profile": the seed panels end by choosing it instead of playing.</summary>
    private Profile? _seedStarting;

    public bool ShowCareerStart => _startRow is not null && ShowPickProfile;

    private BoardCard? StartCard => _startRow?.Card;

    public string CareerStartTitle => _startRow is not { } row ? string.Empty
        : S.Format("careers.start_title", ("game", GameLabel), ("name", row.Gamertag));

    public string CareerStartBody => StartCard is not { } card ? string.Empty
        : S.Format(card.State == CareerState.Empty ? "careers.start_empty" : "careers.start_none", ("game", GameLabel), ("name", _startRow!.Gamertag));

    public bool CareerStartFromExisting => StartCard?.Offers.FromExisting == true;

    public bool CareerStartHardcore => StartCard?.Player.Mode == "hardcore";

    /// <summary>The owner's CR-S52 warning shows with the Hardcore "from an existing profile" option.</summary>
    public bool ShowCareerStartSeedWarning => CareerStartFromExisting && CareerStartHardcore;

    public string CareerStartExistingLabel => S[CareerStartHardcore ? "careers.from_profile" : "careers.from_save"];

    public string CareerStartExistingHint => CareerStartHardcore
        ? S.Format("careers.from_profile_hint", ("sources", string.Join(", ", StartCard!.Offers.Sources.Select(s => ProfileName.Plain(s.Gamertag)))))
        : S["careers.from_save_hint"];

    /// <summary>An empty career (created, never started) can be deleted from the panel (CAREERS.md §6.7: Fresh or delete career).</summary>
    public bool CanDeleteStartCareer => StartCard is { State: CareerState.Empty, CareerRow: not null };

    /// <summary>A card of the Profiles page was clicked: a started career is chosen, one to start opens the Start panel.</summary>
    public void PickProfileRow(ProfileRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        switch (row.Card)
        {
            case null or { Playable: true }:
                ChooseProfile(row.Profile);
                return;
            case { Startable: true }:
                _startRow = row;
                Message = null;
                ChangedCareerStart();
                return;
            case { State: CareerState.NotOffered }:
                Message = S.Format("careers.no_hardcore", ("game", GameLabel));
                return;
            default:
                Message = S.Format("careers.none", ("game", GameLabel));
                return;
        }
    }

    public void CancelCareerStart()
    {
        _startRow = null;
        ChangedCareerStart();
    }

    /// <summary>"Start fresh": the career exists (created or already there), then it is chosen; a Hardcore one starts from scratch (§4.9).</summary>
    public Task StartCareerFreshAsync() => StartCareerAsync(fromExisting: false);

    /// <summary>"Start from an existing profile" (Hardcore, the §4.10 check) or "from a save on this PC" (Noob, the OD-41 picker).</summary>
    public Task StartCareerFromExistingAsync() => StartCareerAsync(fromExisting: true);

    private async Task StartCareerAsync(bool fromExisting)
    {
        if (StartCard is not { } card || _session is null) return;
        if (fromExisting && !card.Offers.FromExisting) return;
        await Run(async () =>
        {
            var row = await EnsureCareerAsync(card);
            if (row is null) return;
            _startRow = null;
            ChangedCareerStart();
            if (!fromExisting)
            {
                Log($"profiles: start {row.Game} {row.ProfileId} fresh");
                if (row.Mode == "hardcore") _seedDeclined.Add(row.ProfileId); // the first Play does not ask again
                ChooseProfile(row);
                Message = S.Format("careers.started", ("game", GameLabel), ("name", ProfileName.Plain(row.Gamertag)));
                return;
            }
            if (row.Mode == "hardcore")
            {
                // §6.7: the source is the other profile's career of the same game, checked by the dry run on the empty career
                var sources = SourcesFor(row.Xuid);
                Log($"profiles: start {row.Game} {row.ProfileId} from {(sources is [var one] ? one.Profile.ProfileId : sources.Count + " sources")}");
                _seedStarting = row;
                _seedChoice = row;
                _seedChoiceSource = sources is [var only] ? only : null;
                ChangedSeedCheck();
                return;
            }
            Log($"profiles: start {row.Game} {row.ProfileId} from a save on this PC");
            ChooseProfile(row);
            OfferSaves(row, always: true); // OD-41: the picker lists this PC's saves of the game (the other profile's included)
        });
    }

    /// <summary>
    /// The career of the card's profile in the chosen game: the existing (empty) one, or a new one from <c>POST /players/{xuid}/careers</c>.
    /// Null when it could not be made (the reason is in <see cref="Message"/> and the log).
    /// </summary>
    private async Task<Profile?> EnsureCareerAsync(BoardCard card)
    {
        if (card.CareerRow is { } existing) return existing;
        if (_session is not { } session) return null;
        var game = CurrentGame.Id;
        var xuid = card.Player.Xuid;
        CareerCreateOutcome outcome = CareerCreateOutcome.Unavailable;
        Exception? error = null;
        for (var attempt = 0; attempt < 2; attempt++)
        {
            if (!_startKeys.TryGetValue((xuid, game), out var key)) _startKeys[(xuid, game)] = key = Guid.CreateVersion7().ToString();
            try
            {
                await session.CreateCareerAsync(xuid, game, key);
                error = null;
            }
            catch (Exception e) when (e is ApiException or HttpRequestException or TaskCanceledException)
            {
                error = e;
            }
            outcome = CareerStart.Classify(error);
            if (outcome != CareerCreateOutcome.KeyReused) break;
            _startKeys.Remove((xuid, game)); // the same key with another body: a new key, one retry
        }
        var code = (error as ApiException)?.Code;
        Log($"profiles: career {game} for {card.Player.ProfileId} → {outcome switch { CareerCreateOutcome.Created => "created", CareerCreateOutcome.Exists => "exists", _ => code ?? error?.GetType().Name ?? outcome.ToString() }}");
        if (outcome != CareerCreateOutcome.Unavailable) _startKeys.Remove((xuid, game)); // answered: the next start is a new request
        switch (outcome)
        {
            case CareerCreateOutcome.Created or CareerCreateOutcome.Exists:
                await session.RefreshMeAsync();
                FillProfiles();
                if (session.Me is { } me && CareerStart.CareerRow(me, xuid, game) is { } row) return row;
                Message = S["careers.not_listed"];
                return null;
            case CareerCreateOutcome.ModeNotAllowed:
                Message = S.Format("careers.no_hardcore", ("game", GameLabel));
                return null;
            case CareerCreateOutcome.GameClosed:
                Message = S.Format("careers.game_closed", ("game", GameLabel));
                return null;
            case CareerCreateOutcome.Banned:
                Message = BannedText((error as ApiException)?.Details);
                return null;
            case CareerCreateOutcome.ProfileGone:
                await session.RefreshMeAsync();
                FillProfiles();
                Message = ErrorText(code ?? ErrorCodes.AccountsNotFound);
                return null;
            case CareerCreateOutcome.Unavailable when error is not ApiException:
                Message = S["native.unreachable"];
                return null;
            default:
                Message = ErrorText(code ?? "HTTP.EMPTY");
                return null;
        }
    }

    /// <summary>The seed panels of a career started from the Start panel end here: the career is chosen (no Play).</summary>
    private void FinishSeedStart(Profile target)
    {
        if (_seedStarting?.ProfileId != target.ProfileId) return;
        _seedStarting = null;
        if (_gate.Game == target.Game && _gate.Step == ShellStep.PickProfile) ChooseProfile(target);
    }

    /// <summary>The refused check's "Delete career": the seed panels close and the typed confirmation opens for the empty career.</summary>
    public void DeleteCareerFromSeed()
    {
        if (_seedOffer?.Target is not { } target) return;
        _seedStarting = null;
        CloseSeedChoice();
        AskDeleteProfile(target);
    }

    /// <summary>The Start panel's "Delete career" (an empty career).</summary>
    public void DeleteStartCareer()
    {
        if (StartCard?.CareerRow is not { } row) return;
        CancelCareerStart();
        AskDeleteProfile(row);
    }

    /// <summary>The refused check names the career or its source: the player may delete the empty career instead (§6.7).</summary>
    public bool ShowSeedDeleteCareer => _seedStarting is not null && _seedOffer is { Result.Outcome: SeedCheckOutcome.Refused or SeedCheckOutcome.SourceUsed };

    /// <summary>This PC has an Azimuth save of the chosen game (a Noob career may start from it, OD-41).</summary>
    private bool HasAzimuthSaves()
    {
        if (_install is null || !CurrentGame.FullFeatures) return false;
        try { return AzimuthSaves.List(AzimuthSaves.SavesRoot(_install.Root, CurrentGame)).Count > 0; }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return false; }
    }

    /// <summary>A card's line: the mode, then where its career of the chosen game stands (CAREERS.md §6.4).</summary>
    private string CareerLine(BoardCard card, Profile row)
    {
        var mode = ModeLine(row);
        var game = GameLabel;
        var state = card.State switch
        {
            CareerState.Empty => S["careers.not_started"],
            CareerState.None => S.Format("careers.none", ("game", game)),
            CareerState.NotOffered => S.Format("careers.no_hardcore", ("game", game)),
            _ => card.ProgressKind switch
            {
                CareerProgressKind.NoSyncYet => S["careers.no_sync"],
                CareerProgressKind.NewCareer => S["careers.new"],
                _ => ProgressText(card.Career!.Progress!),
            },
        };
        // §6.10: until the owner's cleanup, an account may hold more than one profile of a mode; each is listed and says so
        return mode + " · " + state + (card.Duplicate ? " · " + S["careers.duplicate"] : string.Empty);
    }

    private string ProgressText(CareerProgress progress)
    {
        var parts = new List<string>();
        if (progress.Level is { } level) parts.Add(S.Format("careers.level", ("level", level.ToString(CultureInfo.CurrentCulture))));
        if (progress.Credits is { } credits) parts.Add(S.Format("careers.credits", ("credits", credits.ToString("N0", CultureInfo.CurrentCulture))));
        if (progress.Cars is { } cars) parts.Add(S.Format("careers.cars", ("cars", cars.ToString(CultureInfo.CurrentCulture))));
        if (progress.PlaySeconds is { } seconds) parts.Add(S.Format("careers.hours", ("hours", (seconds / 3600).ToString(CultureInfo.CurrentCulture))));
        return parts.Count == 0 ? S["careers.no_sync"] : string.Join(" · ", parts);
    }

    /// <summary>
    /// W12 "download to this PC" (handoff §10): a cloud career's newest revision onto this PC, what its folder holds archived first
    /// (today's <c>fi-launcher-archive</c>, <see cref="CloudSaves.InstallHeadAsync"/>). Only when the cloud is ahead or this PC has
    /// nothing: a PC that moved on, or both, keeps its save (never deleted on a guess); its next Play settles it.
    /// </summary>
    public async Task DownloadCareerAsync(ProfileRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (!row.CanDownload || Playing || _session is not { } session || _install is null || CloudSaveClient() is not { } client
            || ParseXuid(row.Profile) is not { } xuid) return;
        if (System.Diagnostics.Process.GetProcessesByName("azimuth").Length > 0) { Message = S["saves.game_running"]; return; }
        var name = row.Gamertag;
        await Run(async () =>
        {
            var access = await session.Tokens.AccessAsync(CancellationToken.None);
            var (head, failure) = await client.Saves.HeadAsync(access, xuid);
            if (head is null || failure is not null) { Log($"profiles: download {row.Profile.Game} {row.Profile.ProfileId} → {failure?.Code ?? "no head"}"); Message = S["careers.download_failed"]; return; }
            if (head.IsCloudExpired() || head.Revision == 0) { Message = S.Format("careers.download_nothing", ("name", name)); return; }
            var root = AzimuthSaves.SavesRoot(_install.Root, CurrentGame);
            var decision = client.Saves.Decide(root, xuid, head);
            switch (decision.Decision)
            {
                case SyncDecision.InSync:
                    Message = S.Format("careers.download_in_sync", ("name", name));
                    return;
                case SyncDecision.Download or SyncDecision.FastForward:
                    var outcome = await client.Saves.InstallHeadAsync(access, root, xuid, head, decision.LocalContent, "before-download");
                    Log($"profiles: download {row.Profile.Game} {row.Profile.ProfileId} r{head.Revision} → {(outcome.Failure is null ? "installed" : outcome.Failure.Code)}");
                    Message = outcome.Failure is null ? S.Format("careers.downloaded", ("name", name), ("rev", head.Revision.ToString(CultureInfo.CurrentCulture)))
                        : S["careers.download_failed"];
                    return;
                default:
                    Log($"profiles: download {row.Profile.Game} {row.Profile.ProfileId} → kept ({decision.Decision})");
                    Message = S.Format("careers.download_local_newer", ("name", name));
                    return;
            }
        });
    }

    private void ChangedCareerStart()
    {
        Changed(nameof(ShowCareerStart));
        Changed(nameof(CareerStartTitle));
        Changed(nameof(CareerStartBody));
        Changed(nameof(CareerStartFromExisting));
        Changed(nameof(CareerStartHardcore));
        Changed(nameof(ShowCareerStartSeedWarning));
        Changed(nameof(CareerStartExistingLabel));
        Changed(nameof(CareerStartExistingHint));
        Changed(nameof(CanDeleteStartCareer));
    }
}
