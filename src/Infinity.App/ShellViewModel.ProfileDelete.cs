// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// CR-S61 (owner 2026-10-27 "Players delete their own profiles from the cloud, at once, 3 an hour"): "Delete from the cloud" on each
/// account profile, behind a confirmation the player types the profile's name into. The server erases the profile's cloud data at
/// once; this PC's save is archived (never deleted), the local entry and its sync marks go, and /me is read again.
/// </summary>
public sealed partial class ShellViewModel
{
    private Profile? _deleting;
    /// <summary>CR-S98: "Delete profile" (every game, <c>DELETE /players/{xuid}</c>) instead of the chosen game's career.</summary>
    private bool _deleteAllGames;
    private string? _deleteKey;
    private string _deleteTyped = string.Empty;
    private string? _deleteStatus;

    /// <summary>The confirmation panel is open.</summary>
    public bool ShowDeleteProfile => _deleting is not null;

    public string DeleteProfileName => _deleting is null ? string.Empty : ProfileName.Plain(_deleting.Gamertag);

    /// <summary>CR-S98: the panel speaks of careers (a server that lists <c>players</c>); an older server's profile is its one career.</summary>
    private bool DeleteSpeaksCareers => _session?.Me?.Players is not null;

    private string DeleteGameLabel => _deleting is null ? string.Empty : GameTitle(_deleting.Game);

    public string DeleteProfileTitle => _deleting is null ? string.Empty
        : !DeleteSpeaksCareers ? S.Format("profiles.delete_title", ("name", DeleteProfileName))
        : S.Format(_deleteAllGames ? "profiles.delete_all_title" : "profiles.delete_career_title", ("name", DeleteProfileName), ("game", DeleteGameLabel));

    public string DeleteProfileBody => _deleting is null ? string.Empty
        : !DeleteSpeaksCareers ? S.Format("profiles.delete_body", ("name", DeleteProfileName))
        : _deleteAllGames ? S.Format("profiles.delete_all_body", ("name", DeleteProfileName))
        : S.Format("profiles.delete_career_body", ("name", DeleteProfileName), ("game", DeleteGameLabel)) + " "
            + S.Format(_session?.Me is { } me && ProfileDeletion.IsLastCareer(me, _deleting.Xuid, _deleting.Game) ? "profiles.delete_career_last" : "profiles.delete_career_keeps",
                ("name", DeleteProfileName));

    /// <summary>A game's title as the server lists it (else the launcher's catalog, else its id).</summary>
    private string GameTitle(string game) => Games.FirstOrDefault(g => g.Id == game)?.Title ?? Infinity.Core.Process.GameDefinition.ById(game)?.Name ?? game.ToUpperInvariant();

    public string DeleteProfileTypeHint => _deleting is null ? string.Empty : S.Format("profiles.delete_type", ("name", DeleteProfileName));

    /// <summary>What the player typed; "Delete for good" opens only on the profile's plain name.</summary>
    public string DeleteProfileTyped
    {
        get => _deleteTyped;
        set
        {
            _deleteTyped = value ?? string.Empty;
            Changed();
            Changed(nameof(CanConfirmDeleteProfile));
        }
    }

    /// <summary>The refusal of the last attempt, shown in the panel (it stays open so the player can try again).</summary>
    public string? DeleteProfileStatus => _deleteStatus;

    public bool HasDeleteProfileStatus => !string.IsNullOrEmpty(_deleteStatus);

    public bool CanConfirmDeleteProfile => _deleting is not null && Idle && !Playing && _session is not null
        && DeleteConfirm.Matches(_deleteTyped, DeleteProfileName);

    /// <summary>The rows' "Delete from the cloud" links are enabled (never while playing or busy).</summary>
    public bool CanAskDeleteProfile => Idle && !Playing && _session?.Me is not null && !OfflineMode;

    /// <summary>A row's "Delete from the cloud": an account profile only (a local-only row has nothing in the cloud), never while playing.</summary>
    public void AskDeleteProfile(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        if (_session?.Me is not { } me || OfflineMode || Playing || _busy
            || !me.Profiles.Any(p => p.ProfileId == profile.ProfileId && p.Xuid == profile.Xuid))
        {
            return;
        }
        _deleting = profile;
        _deleteAllGames = false;
        _deleteKey = NewDeleteKey(); // kept for every retry of this deletion after a lost answer
        _deleteTyped = string.Empty;
        _deleteStatus = null;
        ChangedDelete();
        SavesGo(SavesScreen.Delete); // SAVE_MANAGER_UX §4: its own screen, with Back
    }

    /// <summary>CR-S98 "Delete profile": every career of the profile, then the profile (<c>DELETE /players/{xuid}</c>), typed as a career.</summary>
    public void AskDeleteWholeProfile(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        // a card without a career of the chosen game stands for its profile: any of its live careers names it
        AskDeleteProfile(_session?.Me?.Profiles.FirstOrDefault(p => p.ProfileId == profile.ProfileId && p.Xuid == profile.Xuid)
            ?? _session?.Me?.Profiles.FirstOrDefault(p => p.Xuid == profile.Xuid && p.Status == "active") ?? profile);
        if (_deleting is null || !DeleteSpeaksCareers) return;
        _deleteAllGames = true;
        ChangedDelete();
    }

    public void CancelDeleteProfile()
    {
        _deleting = null;
        _deleteAllGames = false;
        _deleteKey = null;
        _deleteTyped = string.Empty;
        _deleteStatus = null;
        ChangedDelete();
        SavesPop(SavesScreen.Delete);
    }

    /// <summary>"Delete for good": the DELETE, then this PC's cleanup on 204 or 404.</summary>
    public async Task ConfirmDeleteProfileAsync()
    {
        if (!CanConfirmDeleteProfile || _session is not { } session || _deleting is not { } profile || _deleteKey is not { } key)
        {
            return;
        }
        var allGames = _deleteAllGames && DeleteSpeaksCareers;
        // the careers this deletion removes, read before /me changes (a profile's every game, or the one career)
        var rows = allGames ? session.Me!.Profiles.Where(p => p.Xuid == profile.Xuid).ToList() : [profile];
        var playerId = session.Me?.Players?.FirstOrDefault(p => p.Xuid == profile.Xuid)?.ProfileId ?? profile.ProfileId;
        var label = S[allGames ? "busy.deleting_profile" : "busy.deleting_career"];
        await Run(label, async () =>
        {
            if (allGames) _remote.InvalidatePrefix(RemoteCache.Key("revisions")); else SavesChanged(profile.Game); // owner 2026-11-08: read again when next shown
            async Task<ProfileDeletionOutcome> Send(string idempotencyKey)
            {
                try
                {
                    return allGames ? await session.DeletePlayerAsync(profile.Xuid, idempotencyKey) : await session.DeleteProfileAsync(profile.Xuid, idempotencyKey, game: profile.Game);
                }
                catch (ApiException e) when ((int)e.Status is >= 400 and < 500)
                {
                    _deleteKey = NewDeleteKey(); // answered: the next press is a new request (a lost answer keeps the key)
                    SetDeleteStatus(Describe(e)); // banned, OD-215 rate limits and the rest in the player's words
                    throw;
                }
            }
            var outcome = await Send(key);
            // the last session ended moments ago (its game ticket may be served up to the server's window, ~60 s): say when, wait under
            // this overlay with its own label, then try once more on its own (a new request: the first was answered)
            if (outcome.Result == ProfileDeletionResult.InUse && outcome.FinishingSeconds(DateTimeOffset.UtcNow) is { } seconds
                && seconds <= DeleteFinishingMaxSeconds && ProfileDeletion.InUseGame(outcome) is null)
            {
                Log($"profiles: delete waits {seconds} s for the last session's ticket, then tries once more");
                SetDeleteStatus(S.Format("profiles.delete_finishing", ("seconds", seconds.ToString(CultureInfo.CurrentCulture))));
                for (var left = seconds; left > 0; left--)
                {
                    BusyStep(S.Format("busy.delete_finishing", ("seconds", left.ToString(CultureInfo.CurrentCulture))));
                    await DeleteFinishingTick();
                }
                BusyStep(label);
                _deleteKey = key = NewDeleteKey();
                outcome = await Send(key);
                DeleteRetried++;
            }
            Log(allGames ? $"profiles: delete profile {playerId} → {outcome.Result}"
                : DeleteSpeaksCareers ? $"profiles: delete career {profile.Game} {profile.ProfileId} → {outcome.Result}"
                : $"profiles: delete {profile.ProfileId} → {outcome.Result}");
            if (!outcome.Done)
            {
                _deleteKey = NewDeleteKey();
                SetDeleteStatus(DeleteRefusalText(outcome)); // in the panel, which stays open for another try
                return;
            }
            // each deleted career's save on this PC is archived from its own game's folder, its sync state forgotten for that game only;
            // owner 2026-11-10: the copies, verifications and moves run on a worker (under the busy overlay), never on the window's thread
            var installRoot = _install?.Root;
            var account = session.Me?.AccountId ?? session.Tokens.AccountId;
            _ = StringsIn("en"); _ = StringsIn("pt-BR"); // the history's two reasons, loaded here once
            var steps = rows.Select(row => (Row: row, Game: Infinity.Core.Process.GameDefinition.ById(row.Game) ?? Infinity.Core.Process.GameDefinition.Fh2))
                .Select(s => (s.Row, s.Game, Forget: ForgetSyncFor(s.Game.Id))).ToList();
            var localProfiles = LocalProfiles;
            var fullyArchived = await Task.Run(() =>
            {
                var all = true;
                foreach (var (row, game, forget) in steps)
                {
                    if (ulong.TryParse(row.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var deletedXuid)) ForgetKeptThisPc(game.Id, deletedXuid);
                    ArchiveDeletedCareer(game, row, allGames, installRoot); // SAVE_MANAGER_UX §7.1: deleting is archiving, into the launcher's history first
                    var cleanup = ProfileDeletion.CleanUp(installRoot is null ? null : AzimuthSaves.SavesRoot(installRoot, game), localProfiles,
                        account, row, forget, DateTime.UtcNow);
                    foreach (var path in cleanup.Archived) Log($"profiles: archived {row.Game} " + path);
                    foreach (var path in cleanup.NotArchived) Log($"profiles: not archived (in use) {row.Game} " + path);
                    all &= cleanup.FullyArchived;
                }
                return all;
            });
            var name = ProfileName.Plain(profile.Gamertag);
            CancelDeleteProfile();
            if (rows.Any(r => r.ProfileId == _gate.ProfileKey)) Gate = _gate with { ProfileTag = null, ProfileKey = null }; // pick another profile
            if (_chosenLocal is { } chosen && rows.Any(r => r.ProfileId == chosen.Cloud?.ProfileId)) _chosenLocal = null;
            try { await session.RefreshMeAsync(); }
            finally { FillProfiles(); }
            Message = S.Format(fullyArchived ? "profiles.deleted" : "profiles.deleted_not_archived", ("name", name));
        });
    }

    /// <summary>A refusal of the contract in the player's words (429 and 409 with the wait the server gave).</summary>
    internal string DeleteRefusalText(ProfileDeletionOutcome outcome)
    {
        ArgumentNullException.ThrowIfNull(outcome);
        var minutes = outcome.RetryMinutes?.ToString(CultureInfo.CurrentCulture);
        return outcome.Result switch
        {
            // OD-215: the per-account request/write limits say so (seconds); the deletions' own 3 an hour keeps its text
            ProfileDeletionResult.RateLimited when RateLimits.IsAccountLimit(outcome.Details) =>
                RateText(RateLimits.Describe(RateLimits.Limit(outcome.Details), outcome.RetryAfter)),
            ProfileDeletionResult.RateLimited => minutes is null ? S["profiles.delete_rate_limited_later"] : S.Format("profiles.delete_rate_limited", ("minutes", minutes)),
            // CR-S98 §6.5: a career in use in another game says which
            ProfileDeletionResult.InUse when ProfileDeletion.InUseGame(outcome) is { } game && game != CurrentGame.Id =>
                S.Format("profiles.delete_in_use_game", ("game", GameTitle(game))),
            // the last session is still finishing (details.ticketExpiresAt): say when, in seconds
            ProfileDeletionResult.InUse when outcome.FinishingSeconds(DateTimeOffset.UtcNow) is { } seconds && seconds <= DeleteFinishingMaxSeconds =>
                S.Format("profiles.delete_finishing", ("seconds", seconds.ToString(CultureInfo.CurrentCulture))),
            ProfileDeletionResult.InUse => minutes is null ? S["profiles.delete_in_use_now"] : S.Format("profiles.delete_in_use", ("minutes", minutes)),
            ProfileDeletionResult.DeletionPending => ErrorText(ErrorCodes.AccountsDeletionPending),
            ProfileDeletionResult.Banned => BannedText(outcome.Details),
            _ => string.Empty,
        };
    }

    private static string NewDeleteKey() => Guid.CreateVersion7().ToString();

    /// <summary>The longest wait for a finishing session the delete retries after on its own (the server's window is ~60 s).</summary>
    private const int DeleteFinishingMaxSeconds = 120;

    /// <summary>One second of that wait (the harness makes it instant).</summary>
    internal Func<Task> DeleteFinishingTick { get; set; } = () => Task.Delay(TimeSpan.FromSeconds(1));

    /// <summary>How many automatic retries a delete made (the harness reads it).</summary>
    internal int DeleteRetried { get; private set; }

    /// <summary>The sync-state forget of <paramref name="game"/>'s careers on this server (CR-S98: per game; null without a saves service).</summary>
    private Func<ulong, bool>? ForgetSyncFor(string game)
    {
        if (game == CurrentGame.Id) return CloudSaveClient() is { } client ? client.Saves.Forget : null;
        if (_session?.Info.Services?.Saves is not { } origin || !Uri.TryCreate(origin, UriKind.Absolute, out var saves)) return null;
        var realm = saves.GetLeftPart(UriPartial.Authority);
        return xuid => new SaveSyncStore(_stateDirectory, game).Forget(realm, xuid);
    }

    private void SetDeleteStatus(string? text)
    {
        _deleteStatus = text;
        Changed(nameof(DeleteProfileStatus));
        Changed(nameof(HasDeleteProfileStatus));
    }

    private void ChangedDelete()
    {
        Changed(nameof(ShowDeleteProfile));
        Changed(nameof(DeleteProfileName));
        Changed(nameof(DeleteProfileTitle));
        Changed(nameof(DeleteProfileBody));
        Changed(nameof(DeleteProfileTypeHint));
        Changed(nameof(DeleteProfileTyped));
        Changed(nameof(DeleteProfileStatus));
        Changed(nameof(HasDeleteProfileStatus));
        Changed(nameof(CanConfirmDeleteProfile));
    }
}
