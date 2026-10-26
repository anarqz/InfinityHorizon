// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;
using Infinity.Core.Shell;
using Infinity.Core.Saves;

namespace Infinity.App;

/// <summary>
/// OD-52: only an Infinity Supporter's Hardcore profile keeps data on the servers (the cloud save,
/// the profile sync, the universe). OD-60: a Hardcore profile starts empty, and the game seeds it
/// on the first launch (the opening race again, the starter car picked in the game); its first
/// cloud revision is that career, uploaded at exit. A Noob one starts from the player's offline
/// save (OD-41). Free accounts create Noob profiles; Hardcore → Noob is one-way.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _newHardcore;

    /// <summary>The account is an Infinity Supporter (internal tier <c>vip</c>): Hardcore can be chosen.</summary>
    public bool CanChooseHardcore => _session?.Me?.Access.Tier == "vip";

    public bool NewHardcore
    {
        get => _newHardcore && CanChooseHardcore;
        set
        {
            _newHardcore = value;
            Changed(nameof(NewHardcore));
            Changed(nameof(NewNoob));
            Changed(nameof(AzimuthNameWarning));
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
            Message = S.Format("modes.converted", ("name", ProfileName.Plain(profile.Gamertag)));
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

    /// <summary>OD-52 / OD-60: the cloud save around a Play (only a Supporter's Hardcore line does anything).</summary>
    private SessionSaves? SessionSavesFor(Profile profile)
    {
        if (_install is null || _session is not { } session || CloudSaveClient() is not { } client || !ulong.TryParse(profile.Xuid, out var xuid))
        {
            return null;
        }
        return new SessionSaves(client.Saves, client.Api, ct => session.Tokens.AccessAsync(ct), AzimuthSaves.SavesRoot(_install.Root), xuid,
            (step, detail) => _ui.Post(_ => RenderSave(step, detail), null));
    }

    private void RenderSave(SaveStep step, string? detail)
    {
        Log($"save: {step} {detail}");
        var text = step switch
        {
            SaveStep.Seeding => S["saves.seeding"],
            SaveStep.Downloading => S["saves.downloading"],
            SaveStep.Installed => S.Format("saves.installed", ("rev", detail ?? "?")),
            SaveStep.Uploading => S["saves.uploading"],
            SaveStep.Uploaded => S.Format("saves.uploaded", ("rev", detail ?? "?")),
            SaveStep.Kept => S["saves.kept"],
            SaveStep.Refused => S["saves.refused"],
            _ => null,
        };
        if (text is not null)
        {
            SetStatus(text);
        }
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
            return "NOOB · " + (CanChooseHardcore ? S["modes.noob_supporter"] : S["profiles.local_saves"]);
        }
        return "HARDCORE · " + (CanChooseHardcore ? S["modes.hardcore_cloud"] : S["modes.hardcore_lapsed"]);
    }
}

/// <summary>One row of the profile picker.</summary>
public sealed record ProfileRow(Profile Profile, string Line)
{
    public string Gamertag => ProfileName.Plain(Profile.Gamertag);

    public bool Hardcore => Profile.Mode == "hardcore";
}
