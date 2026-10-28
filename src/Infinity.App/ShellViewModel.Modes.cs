// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;
using Infinity.Core.Shell;
using Infinity.Core.Saves;

namespace Infinity.App;

/// <summary>
/// Which profiles keep data on the servers (the cloud save, the profile sync, the universe) is the
/// server's cloud lines (CR-S51, OD-128); the tier is only a badge (CR-S69). OD-60: a Hardcore profile starts empty, and the game seeds it
/// on the first launch (the opening race again, the starter car picked in the game); its first
/// cloud revision is that career, uploaded at exit. A Noob one starts from the player's offline
/// save (OD-41). Every account creates either; Hardcore → Noob is one-way.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _newHardcore;

    /// <summary>
    /// Signed in to an account: Hardcore can be chosen on every tier (owner 2026-10-27 "Follow the server's flags"); whether it keeps
    /// a cloud line is the server's (CR-S51 <see cref="CloudLines"/>), the admission decides.
    /// </summary>
    public bool CanChooseHardcore
    {
        get { var can = _session?.Me is not null; InspectCanChooseHardcore(ref can); return can; }
    }

    partial void InspectCanChooseHardcore(ref bool can);

    /// <summary>CR-S69: the account's badge (and which of the server's cloud lines is its), never a gate.</summary>
    private bool IsSupporter => _session?.Me?.Access.Tier == "vip";

    /// <summary>CR-S51: this server keeps a new Hardcore profile of this account in the cloud (absent policy = OD-52, a Supporter's).</summary>
    public bool HardcoreKeepsCloud => IsSupporter ? _cloudLines?.HardcoreVip ?? true : _cloudLines?.HardcoreFree ?? false;

    public string HardcoreHint => S[HardcoreKeepsCloud ? "modes.hardcore_hint" : "modes.hardcore_hint_local"];

    /// <summary>CR-S66 (OD-128): this server keeps a Noob profile of this account in the cloud, saved without the checks (absent policy: none).</summary>
    public bool NoobKeepsCloud => IsSupporter ? _cloudLines?.NoobVip ?? false : _cloudLines?.NoobFree ?? false;

    public string NoobHint => S[NoobKeepsCloud ? "modes.noob_hint_cloud" : "modes.noob_hint"];

    public string FreeNoobHint => S[NoobKeepsCloud ? "modes.free_noob_cloud" : "modes.free_noob"];

    public string ConvertKeeps => S[NoobKeepsCloud ? "modes.convert_keeps_cloud" : "modes.convert_keeps"];

    public bool NewHardcore
    {
        get => _newHardcore && CanChooseHardcore;
        set
        {
            _newHardcore = value;
            Changed(nameof(NewHardcore));
            Changed(nameof(NewNoob));
            Changed(nameof(AzimuthNameWarning));
            ChangedSeedForm(); // CR-S52: "Start from one of my profiles" is a Hardcore option
            ChangedNoobSeedForm(); // owner 2026-10-28: and "Start from one of my Hardcore profiles" a Noob one
        }
    }

    public bool NewNoob
    {
        get => !NewHardcore;
        set => NewHardcore = !value;
    }

    private Profile? _converting;
    private ModeChangeConfirm? _convertDetails;
    private string? _convertKey;

    /// <summary>The Hardcore → Noob warning is open (step 1 answered).</summary>
    public bool ShowConvert => _converting is not null && _convertDetails is not null;

    public string ConvertTitle => _converting is null ? string.Empty : S.Format("modes.convert_title", ("name", ProfileName.Plain(_converting.Gamertag)));

    /// <summary>What the change deletes for good, one line per kind the server counted.</summary>
    public string ConvertDeletes
    {
        get
        {
            if (_convertDetails is null)
            {
                return string.Empty;
            }
            var lines = _convertDetails.Deletes.Where(d => d.Value > 0)
                .Select(d => "• " + d.Value.ToString("N0", CultureInfo.CurrentCulture) + " " + KindLabel(d.Key));
            var text = string.Join(Environment.NewLine, lines);
            return text.Length == 0 ? S["modes.convert_nothing"] : text;
        }
    }

    private string KindLabel(string kind) => kind switch
    {
        "scores" => S["modes.kind_scores"],
        "ghosts" => S["modes.kind_ghosts"],
        "xp" => S["modes.kind_xp"],
        "drivatars" => S["modes.kind_drivatars"],
        "onlineProfile" => S["modes.kind_online_profile"],
        "inbox" => S["modes.kind_inbox"],
        "saveRevisions" => S["modes.kind_save_revisions"],
        _ => kind,
    };

    /// <summary>Hardcore → Noob, step 1: ask the server what it would delete (nothing is written).</summary>
    public async Task AskConvertAsync(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        if (_session is null || profile.Mode != "hardcore" || Playing)
        {
            return;
        }
        await Run(async () =>
        {
            var details = await _session.AskNoobAsync(profile.Xuid);
            if (details is null)
            {
                await _session.RefreshMeAsync();
                FillProfiles();
                return;
            }
            CancelDeleteProfile(); // one confirmation at a time (CR-S61)
            _converting = profile;
            _convertDetails = details;
            _convertKey = Guid.CreateVersion7().ToString(); // reused on every retry of the confirmation
            ChangedConvert();
        });
    }

    /// <summary>Hardcore → Noob, step 2: the player confirmed. The newest cloud save must be on the PC first.</summary>
    public async Task ConfirmConvertAsync()
    {
        if (_session is null || _converting is not { } profile || _convertDetails is not { } details || _convertKey is not { } key)
        {
            return;
        }
        await Run(async () =>
        {
            if (details.SaveRevision > 0 && !await DownloadNewestAsync(profile))
            {
                // the cloud save would be gone for good: it comes to this PC first (SAVES_API.md §4)
                Message = S["modes.convert_needs_download"];
                return;
            }
            await _session.ConfirmNoobAsync(profile.Xuid, key);
            CancelConvert();
            await _session.RefreshMeAsync();
            FillProfiles();
            Message = S.Format(NoobKeepsCloud ? "modes.converted_cloud" : "modes.converted", ("name", ProfileName.Plain(profile.Gamertag)));
        });
    }

    public void CancelConvert()
    {
        _converting = null;
        _convertDetails = null;
        _convertKey = null;
        ChangedConvert();
    }

    private void ChangedConvert()
    {
        Changed(nameof(ShowConvert));
        Changed(nameof(ConvertTitle));
        Changed(nameof(ConvertDeletes));
    }

    private CloudSaves? _cloudSaves;
    private SavesApiClient? _savesApi;

    /// <summary>The save client of this server (one per launcher: its state file is per realm and profile).</summary>
    private (CloudSaves Saves, SavesApiClient Api)? CloudSaveClient()
    {
        if (_session?.Info.Services?.Saves is not { } origin || !Uri.TryCreate(origin, UriKind.Absolute, out var saves))
        {
            return null;
        }
        _savesApi ??= new SavesApiClient(Http, saves, ClientHeader);
        _cloudSaves ??= new CloudSaves(_savesApi, new SaveSyncStore(_stateDirectory), saves.GetLeftPart(UriPartial.Authority));
        return (_cloudSaves, _savesApi);
    }

    /// <summary>OD-52 / OD-60: the cloud save around a Play (only a profile with a cloud line does anything; the admission says).</summary>
    private SessionSaves? SessionSavesFor(Profile profile)
    {
        if (_install is null || _session is not { } session || CloudSaveClient() is not { } client || !ulong.TryParse(profile.Xuid, out var xuid))
        {
            return null;
        }
        return new SessionSaves(client.Saves, client.Api, ct => session.Tokens.AccessAsync(ct), AzimuthSaves.SavesRoot(_install.Root), xuid,
            (step, detail) => _ui.Post(_ => RenderSave(step, detail), null))
        {
            ResolveConflict = AskSaveConflictAsync,
            Checkpoints = session.Info.Checkpoints, // OD122-L3: the server's cadence while the game runs
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

    public bool ShowSaveConflict => _saveConflict is not null;

    public bool CanKeepCloudSave => _saveConflict?.CanKeepCloud == true;

    public string SaveConflictCloudText => _saveConflict is not { } c ? string.Empty : SaveSideText(
        c.Cloud.Summary?.Level is { } level ? (int?)level : null, c.Cloud.Summary?.Credits is { } credits ? (int?)credits : null,
        DateTime.TryParse(c.Cloud.CommittedAt, CultureInfo.InvariantCulture, DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var when) ? when : null);

    public string SaveConflictLocalText => _saveConflict is not { } c ? string.Empty : SaveSideText(c.LocalLevel, c.LocalCredits, c.LocalSavedUtc);

    private string SaveSideText(int? level, int? credits, DateTime? when)
    {
        var parts = new List<string>();
        if (level is { } l) parts.Add(S.Format("saves.level", ("level", l.ToString(CultureInfo.CurrentCulture))));
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
            SaveStep.SeedAccepted => S.Format("saves.seed_accepted", ("rev", detail ?? "?")),
            SaveStep.SeedRefused => S[ProfileSeed.CommitTextKey(detail)], // refused, not sent, unconfirmed or the account's review
            _ => null,
        };
        if (text is not null)
        {
            SetStatus(text);
        }
        // CR-S52: the fall back to a new career is told where it stays on screen, not only on the status line the next steps replace
        if (step == SaveStep.SeedRefused) Message = text;
    }

    /// <summary>Hardcore → Noob: the line's newest cloud revision onto this PC first (no lease needed to read).</summary>
    private async Task<bool> DownloadNewestAsync(Profile profile)
    {
        if (_install is null || _session is null || CloudSaveClient() is not { } client || !ulong.TryParse(profile.Xuid, out var xuid))
        {
            return false;
        }
        var access = await _session.Tokens.AccessAsync();
        var (head, failure) = await client.Saves.HeadAsync(access, xuid);
        if (head is null || failure is not null)
        {
            return false;
        }
        var root = AzimuthSaves.SavesRoot(_install.Root);
        if (head.IsCloudExpired())
        {
            RenderSave(SaveStep.CloudExpired, null); // retention: nothing to bring down; this PC's save is the one kept
            return true;
        }
        var decision = client.Saves.Decide(root, xuid, head);
        switch (decision.Decision)
        {
            case SyncDecision.InSync or SyncDecision.Empty:
                return true;
            case SyncDecision.Download or SyncDecision.FastForward:
                var outcome = await client.Saves.InstallHeadAsync(access, root, xuid, head, decision.LocalContent, "before-noob");
                return outcome.Failure is null;
            default:
                return false; // this PC is ahead or both moved: never delete progress on a guess
        }
    }

    /// <summary>What a profile row says under its gamertag: its mode, and what that means now.</summary>
    public string ModeLine(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        if (profile.Mode != "hardcore")
        {
            // CR-S66: a Noob line is saved at once (no checks); without one, what this server does for Noob
            return "NOOB · " + (profile.CloudLine == true ? S["modes.noob_cloud"]
                : profile.CloudLine is null && NoobKeepsCloud ? S["modes.noob_cloud"] : S["profiles.local_saves"]);
        }
        // CR-S51: the server says whether this profile keeps a cloud line; without it (an older server), what this server's
        // cloud lines say for the account (CR-S69: never a Supporter-only wording)
        return "HARDCORE · " + S[(profile.CloudLine ?? HardcoreKeepsCloud) ?"modes.hardcore_cloud" : "modes.hardcore_local"];
    }
}

/// <summary>One row of the profile picker. <see cref="OnAccount"/>: a profile of the signed-in account (not a local-only row).</summary>
public sealed record ProfileRow(Profile Profile, string Line, bool OnAccount = false)
{
    public string Gamertag => ProfileName.Plain(Profile.Gamertag);

    public bool Hardcore => Profile.Mode == "hardcore";

    /// <summary>CR-S61: "Delete from the cloud" is offered on account profiles only.</summary>
    public bool CanDeleteFromCloud => OnAccount;
}
