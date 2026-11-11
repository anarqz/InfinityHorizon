// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;
using Infinity.Core.Shell;
using Infinity.Core.Saves;

namespace Infinity.App;

/// <summary>
/// <summary>
/// Which profiles keep data on the servers is the server's (CR-S51, OD-128); the tier is only a badge (CR-S69). OD-60: a Hardcore profile
/// starts empty, and the game seeds it on the first launch (the opening race again, the starter car picked in the game); its first cloud
/// revision is that career, uploaded at exit. OD-239: an Azimuth profile (wire mode <c>noob</c>) plays the player's own Azimuth save and
/// has no cloud save at all: nothing of it is downloaded, uploaded or analysed.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _newHardcore;

    /// <summary>
    /// Signed in to an account: Hardcore can be chosen on every tier (owner 2026-10-27 "Follow the server's flags"); whether it keeps
    /// a cloud line is the server's (CR-S51), the admission decides.
    /// </summary>
    public bool CanChooseHardcore
    {
        // OD-206 (plan A1): a game without FH2's features has no Hardcore; the server refuses one (400 profile.mode), and the refused
        // profile would stay on this PC only, playable offline alone (T1, 2026-11-07)
        // OD-214: a game whose careers allow no Hardcore (FM5 until M11) offers none; an account with a live Hardcore profile makes no second
        get { var can = _session?.Me is not null && CurrentGame.FullFeatures && CurrentGame.HardcoreCareers && !HasLiveProfile("hardcore"); InspectCanChooseHardcore(ref can); return can; }
    }

    /// <summary>OD-214 / CAREERS.md §2: a new Azimuth profile only while the account has no live one (it gets a career of this game instead).</summary>
    public bool CanChooseNoob => !HasLiveProfile("noob");

    /// <summary>The new-profile form: offline always; signed in only while a mode has no live profile the game allows.</summary>
    public bool ShowNewProfile => OfflineMode || _session?.Me?.Players is null || CanChooseNoob || CanChooseHardcore;

    /// <summary>A CR-S98 server lists a live profile of <paramref name="mode"/> on this account (<c>players</c>).</summary>
    private bool HasLiveProfile(string mode) =>
        !OfflineMode && _session?.Me?.Players is { } players && players.Any(p => p.Mode == mode && p.Status == "active");

    partial void InspectCanChooseHardcore(ref bool can);

    /// <summary>CR-S69: the account's badge (and which of the server's Hardcore cloud lines is its), never a gate.</summary>
    private bool IsSupporter => _session?.Me?.Access.Tier == "vip";

    /// <summary>CR-S51: this server keeps a new Hardcore profile of this account in the cloud (absent policy = OD-52, a Supporter's).</summary>
    public bool HardcoreKeepsCloud => IsSupporter ? _cloudLines?.HardcoreVip ?? true : _cloudLines?.HardcoreFree ?? false;

    public string HardcoreHint => S[HardcoreKeepsCloud ? "modes.hardcore_hint" : "modes.hardcore_hint_local"];

    /// <summary>OD-239: an Azimuth profile plays the player's own Azimuth save; there is no cloud wording to choose.</summary>
    public string NoobHint => S["modes.noob_hint"];

    public string FreeNoobHint => S["modes.free_noob"];

    public bool NewHardcore
    {
        get => (_newHardcore || !CanChooseNoob) && CanChooseHardcore;
        set
        {
            _newHardcore = value;
            Changed(nameof(NewHardcore));
            Changed(nameof(NewNoob));
            Changed(nameof(AzimuthNameWarning));
            ChangedSeedForm(); // CR-S52: "Start from one of my profiles" is a Hardcore option
        }
    }

    public bool NewNoob
    {
        get => !NewHardcore;
        set => NewHardcore = !value;
    }

    private readonly Dictionary<(string Game, string? Query), (CloudSaves Saves, SavesApiClient Api)> _saveClients = [];

    /// <summary>
    /// The save client of this server for the chosen game (CR-S98: its state is per realm, game and XUID, and every route carries
    /// <c>?game=</c> on a server that knows careers; none on an older one).
    /// </summary>
    private (CloudSaves Saves, SavesApiClient Api)? CloudSaveClient()
    {
        if (_session?.Info.Services?.Saves is not { } origin || !Uri.TryCreate(origin, UriKind.Absolute, out var saves))
        {
            return null;
        }
        var game = CurrentGame.Id;
        var query = _session.CareerGame(game);
        if (!_saveClients.TryGetValue((game, query), out var client))
        {
            // CR-S105: the line's bundle is the game's (FH2SAVE1, or FM5SAVE1 with its garage's containers)
            var bundle = CurrentGame.Saves.Bundle;
            var api = new SavesApiClient(Http, saves, ClientHeader, query, bundle);
            client = (new CloudSaves(api, new SaveSyncStore(_stateDirectory, game, bundle.MaxBundle), saves.GetLeftPart(UriPartial.Authority), bundle), api);
            _saveClients[(game, query)] = client;
        }
        return client;
    }

    /// <summary>
    /// OD-52 / OD-60: the cloud save around a Play (only a profile with a cloud line does anything; the admission says). OD-239: none for an
    /// Azimuth profile, whatever the gate answers.
    /// </summary>
    private SessionSaves? SessionSavesFor(Profile profile)
    {
        if (profile.Mode != "hardcore" || !CurrentGame.CloudSaves || _install is null || _session is not { } session || CloudSaveClient() is not { } client || !ulong.TryParse(profile.Xuid, out var xuid))
        {
            return null;
        }
        var game = CurrentGame.Id;
        return new SessionSaves(client.Saves, client.Api, ct => session.Tokens.AccessAsync(ct), AzimuthSaves.SavesRoot(_install.Root, CurrentGame), xuid,
            (step, detail) =>
            {
                if (step is SaveStep.Uploaded) ForgetKeptThisPc(game, xuid); // the kept save is in the cloud: the choice is spent
                _ui.Post(_ => { TrackSaveUpload(game, xuid, step); RenderSave(step, detail); }, null);
            })
        {
            ResolveConflict = AskSaveConflictAsync,
            // Saves › Resolve "Keep this PC" for exactly this cloud head and this PC's save: committed without asking again
            KeptThisPc = conflict => KeptThisPcFor(game, xuid, conflict.Cloud.Revision, conflict.LocalContent) is not null,
            Checkpoints = session.Info.Checkpoints, // OD122-L3: the server's cadence while the game runs
            Note = Log, // OD-133: the fresh-start guard's decisions
            ResolveRefusedFreshStart = AskRefusedFreshStartAsync, // OD-133: a first save refused as past its start
        };
    }

    private SaveConflict? _saveConflict;
    private TaskCompletionSource<ConflictChoice>? _saveConflictAnswer;

    /// <summary>
    /// Owner 2026-10-26 ("Cloud or PC, per the contract"): this PC and the cloud both moved on; the player picks. Nothing heartbeats
    /// the lease before the game starts, so the question lasts <see cref="SaveConflictWait"/> (well inside the lease); after it, closing
    /// it or cancelling the Play, the answer is "not now".
    /// </summary>
    private static readonly TimeSpan SaveConflictWait = TimeSpan.FromMinutes(3);

    private Task<ConflictChoice> AskSaveConflictAsync(SaveConflict conflict, CancellationToken cancellation)
    {
        var answer = new TaskCompletionSource<ConflictChoice>(TaskCreationOptions.RunContinuationsAsynchronously);
        // a cancelled Play is "not now", and the question leaves the screen with it
        var registration = cancellation.Register(() =>
        {
            if (answer.TrySetResult(ConflictChoice.NotNow))
                _ui.Post(_ => { if (ReferenceEquals(_saveConflictAnswer, answer)) { _saveConflict = null; _saveConflictAnswer = null; Changed(string.Empty); } }, null);
        });
        _ = answer.Task.ContinueWith(_ => registration.Dispose(), TaskScheduler.Default);
        _ = Task.Delay(SaveConflictWait, cancellation).ContinueWith(_ =>
        {
            if (answer.TrySetResult(ConflictChoice.NotNow))
                _ui.Post(__ => { if (ReferenceEquals(_saveConflictAnswer, answer)) { _saveConflict = null; _saveConflictAnswer = null; Message = S["saves.conflict_timeout"]; Changed(string.Empty); } }, null);
        }, TaskScheduler.Default);
        _ui.Post(_ =>
        {
            _saveConflictAnswer?.TrySetResult(ConflictChoice.NotNow);
            _saveConflict = conflict;
            _saveConflictAnswer = answer;
            Log($"save: conflict on revision {conflict.Cloud.Revision}; asking the player");
            Changed(string.Empty);
        }, null);
        return answer.Task;
    }

    private FreshStartRefusal? _freshRefusal;
    private TaskCompletionSource<FreshStartChoice>? _freshRefusalAnswer;

    /// <summary>
    /// OD-133: the line's first save was refused because the career is past its start. Asked before the game starts, like the conflict
    /// screen (same wait); closing it, cancelling the Play or the wait running out is "not now": nothing archived, nothing sent.
    /// </summary>
    private Task<FreshStartChoice> AskRefusedFreshStartAsync(FreshStartRefusal refusal, CancellationToken cancellation)
    {
        var answer = new TaskCompletionSource<FreshStartChoice>(TaskCreationOptions.RunContinuationsAsynchronously);
        void Clear() => _ui.Post(_ => { if (ReferenceEquals(_freshRefusalAnswer, answer)) { _freshRefusal = null; _freshRefusalAnswer = null; Changed(string.Empty); } }, null);
        var registration = cancellation.Register(() => { if (answer.TrySetResult(FreshStartChoice.NotNow)) Clear(); });
        _ = answer.Task.ContinueWith(_ => registration.Dispose(), TaskScheduler.Default);
        _ = Task.Delay(SaveConflictWait, cancellation).ContinueWith(_ => { if (answer.TrySetResult(FreshStartChoice.NotNow)) Clear(); }, TaskScheduler.Default);
        _ui.Post(_ =>
        {
            _freshRefusalAnswer?.TrySetResult(FreshStartChoice.NotNow);
            _freshRefusal = refusal;
            _freshRefusalAnswer = answer;
            Log($"save: first save refused ({refusal.Rule}); asking the player");
            Changed(string.Empty);
        }, null);
        return answer.Task;
    }

    public bool ShowFreshStartRefused => _freshRefusal is not null;

    /// <summary>The player's answer to the refused first save (the panel's two buttons).</summary>
    public void AnswerFreshStartRefused(FreshStartChoice choice)
    {
        if (_freshRefusalAnswer is not { } answer) return;
        Log("save: refused first save answered " + choice);
        _freshRefusal = null;
        _freshRefusalAnswer = null;
        answer.TrySetResult(choice);
        Changed(string.Empty);
    }

    public bool ShowSaveConflict => _saveConflict is not null;

    public bool CanKeepCloudSave => _saveConflict?.CanKeepCloud == true;

    public string SaveConflictCloudText => _saveConflict is not { } c ? string.Empty : SaveSideText(
        SaveSummary.LevelText(S, c.Cloud.Summary), c.Cloud.Summary?.Credits is { } credits ? (int?)credits : null,
        DateTime.TryParse(c.Cloud.CommittedAt, CultureInfo.InvariantCulture, DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var when) ? when : null);

    // This PC's side has no level: ProfileData's Level is not the driver level (CR-S81); only the server derives it.
    public string SaveConflictLocalText => _saveConflict is not { } c ? string.Empty : SaveSideText(null, c.LocalCredits, c.LocalSavedUtc);

    private string SaveSideText(string? level, int? credits, DateTime? when)
    {
        var parts = new List<string>();
        if (level is { } l) parts.Add(l);
        if (credits is { } cr) parts.Add(S.Format("saves.credits", ("credits", cr.ToString("N0", CultureInfo.CurrentCulture))));
        if (when is { } w) parts.Add(S.Format("saves.last_played", ("when", w.ToLocalTime().ToString("g", CultureInfo.CurrentCulture))));
        return parts.Count == 0 ? S["saves.conflict_unknown"] : string.Join(" · ", parts);
    }

    public void AnswerSaveConflict(ConflictChoice choice)
    {
        if (_saveConflictAnswer is not { } answer) return;
        Log("save: conflict answered " + choice);
        _saveConflict = null;
        _saveConflictAnswer = null;
        answer.TrySetResult(choice);
        Changed(string.Empty);
    }

    private void RenderSave(SaveStep step, string? detail)
    {
        Log($"save: {step} {detail}");
        var text = step switch
        {
            SaveStep.Seeding => S["saves.seeding"],
            SaveStep.FreshStart => S["saves.fresh_start"],
            SaveStep.CloudExpired => S["saves.cloud_expired"],
            SaveStep.Downloading => S["saves.downloading"],
            SaveStep.Installed => S.Format("saves.installed", ("rev", detail ?? "?")),
            SaveStep.Uploading => S["saves.uploading"],
            SaveStep.Uploaded => S.Format("saves.uploaded", ("rev", detail ?? "?")),
            SaveStep.Kept => S["saves.kept"],
            SaveStep.Refused => S["saves.refused"],
            SaveStep.FreshStartHeld => S["saves.fresh_refused_held"],
            SaveStep.RestoreRefused => detail == "LOCAL.SET_ASIDE" ? S["restore.set_aside_done"]
                : CareerRestore.IsCeiling(detail) ? S["restore.commit_refused_ceiling"]
                : detail?.Contains('.', StringComparison.Ordinal) == true ? S.Format("restore.commit_failed_undone", ("code", detail))
                : S["restore.commit_refused"],
            SaveStep.RestoreUndoFailed => S["restore.undo_failed"],
            SaveStep.SeedAccepted => S.Format("saves.seed_accepted", ("rev", detail ?? "?")),
            SaveStep.SeedRefused => S[ProfileSeed.CommitTextKey(detail)], // refused, not sent, unconfirmed or the account's review
            _ => null,
        };
        if (text is not null)
        {
            SetStatus(text);
        }
        // CR-S52: the fall back to a new career is told where it stays on screen, not only on the status line the next steps replace
        if (step is SaveStep.SeedRefused or SaveStep.RestoreRefused or SaveStep.RestoreUndoFailed or SaveStep.FreshStartHeld) Message = text;
    }

    /// <summary>What a profile row says under its gamertag: its mode, and what that means now.</summary>
    public string ModeLine(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        if (profile.Mode != "hardcore")
        {
            // OD-239: an Azimuth profile plays the player's own Azimuth save, never a cloud line (whatever an older server says)
            return S["sm.noob"] + " · " + S["modes.noob_own_save"];
        }
        // CR-S51: the server says whether this profile keeps a cloud line; without it (an older server), what this server's
        // cloud lines say for the account (CR-S69: never a Supporter-only wording)
        return "HARDCORE · " + S[(profile.CloudLine ?? HardcoreKeepsCloud) ?"modes.hardcore_cloud" : "modes.hardcore_local"];
    }
}

/// <summary>
/// One row of the profile picker. <see cref="OnAccount"/>: a profile of the signed-in account (not a local-only row). <see cref="Card"/>
/// (OD-214, a CR-S98 server): the profile and its career of the chosen game; null for a local row or an older server.
/// </summary>
public sealed record ProfileRow(Profile Profile, string Line, bool OnAccount = false, BoardCard? Card = null)
{
    public string Gamertag => ProfileName.Plain(Profile.Gamertag);

    public bool Hardcore => Profile.Mode == "hardcore";

    /// <summary>CR-S61: "Delete from the cloud" is offered on account profiles only (an older server's: the profile is its one career).</summary>
    public bool CanDeleteFromCloud => OnAccount && Card is null;

    /// <summary>CR-S98: "Delete &lt;game&gt; career" on a profile with a career of the chosen game.</summary>
    public bool CanDeleteCareer => OnAccount && Card?.CareerRow is not null;

    /// <summary>CR-S98: "Delete profile" (every game) on every account profile of a server that lists them.</summary>
    public bool CanDeleteWholeProfile => OnAccount && Card is not null;

    /// <summary>W12: "Download to this PC" on a started Hardcore career the cloud holds a revision of (OD-239: never an Azimuth one).</summary>
    public bool CanDownload => OnAccount && Hardcore && Card is { State: CareerState.Started, Career.Cloud.Revision: > 0 };

    /// <summary>The career link's words, with the game's title.</summary>
    public string DeleteCareerLabel { get; init; } = string.Empty;

    /// <summary>OD-214: the card opens the Start panel (no career of the game, or an empty one).</summary>
    public bool ShowStart => Card is { Startable: true };
}
