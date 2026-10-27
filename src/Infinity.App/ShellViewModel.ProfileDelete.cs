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
    private string? _deleteKey;
    private string _deleteTyped = string.Empty;
    private string? _deleteStatus;

    /// <summary>The confirmation panel is open.</summary>
    public bool ShowDeleteProfile => _deleting is not null;

    public string DeleteProfileName => _deleting is null ? string.Empty : ProfileName.Plain(_deleting.Gamertag);

    public string DeleteProfileTitle => _deleting is null ? string.Empty : S.Format("profiles.delete_title", ("name", DeleteProfileName));

    public string DeleteProfileBody => _deleting is null ? string.Empty : S.Format("profiles.delete_body", ("name", DeleteProfileName));

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

    public bool CanConfirmDeleteProfile => _deleting is not null && Idle && !Playing && !DedicatedActive && _session is not null
        && string.Equals(_deleteTyped.Trim(), DeleteProfileName, StringComparison.OrdinalIgnoreCase);

    /// <summary>The rows' "Delete from the cloud" links are enabled (never while playing, hosting or busy).</summary>
    public bool CanAskDeleteProfile => Idle && !Playing && !DedicatedActive && _session?.Me is not null && !OfflineMode;

    /// <summary>A row's "Delete from the cloud": an account profile only (a local-only row has nothing in the cloud), never while playing.</summary>
    public void AskDeleteProfile(Profile profile)
    {
        ArgumentNullException.ThrowIfNull(profile);
        if (_session?.Me is not { } me || OfflineMode || Playing || _busy || DedicatedActive
            || !me.Profiles.Any(p => p.ProfileId == profile.ProfileId && p.Xuid == profile.Xuid))
        {
            return;
        }
        CancelConvert();
        _deleting = profile;
        _deleteKey = NewDeleteKey(); // kept for every retry of this deletion after a lost answer
        _deleteTyped = string.Empty;
        _deleteStatus = null;
        ChangedDelete();
    }

    public void CancelDeleteProfile()
    {
        _deleting = null;
        _deleteKey = null;
        _deleteTyped = string.Empty;
        _deleteStatus = null;
        ChangedDelete();
    }

    /// <summary>"Delete for good": the DELETE, then this PC's cleanup on 204 or 404.</summary>
    public async Task ConfirmDeleteProfileAsync()
    {
        if (!CanConfirmDeleteProfile || _session is not { } session || _deleting is not { } profile || _deleteKey is not { } key)
        {
            return;
        }
        await Run(async () =>
        {
            ProfileDeletionOutcome outcome;
            try
            {
                outcome = await session.DeleteProfileAsync(profile.Xuid, key);
            }
            catch (ApiException e) when ((int)e.Status is >= 400 and < 500)
            {
                _deleteKey = NewDeleteKey(); // answered: the next press is a new request (a lost answer keeps the key)
                SetDeleteStatus(e.Code == ErrorCodes.AccountsBanned ? BannedText(e.Details) : ErrorText(e.Code));
                throw;
            }
            Log($"profiles: delete {profile.ProfileId} → {outcome.Result}");
            if (!outcome.Done)
            {
                _deleteKey = NewDeleteKey();
                SetDeleteStatus(DeleteRefusalText(outcome)); // in the panel, which stays open for another try
                return;
            }
            var cleanup = ProfileDeletion.CleanUp(_install is null ? null : AzimuthSaves.SavesRoot(_install.Root), LocalProfiles, session.Me?.AccountId ?? session.Tokens.AccountId,
                profile, CloudSaveClient() is { } client ? client.Saves.Forget : null, DateTime.UtcNow);
            foreach (var path in cleanup.Archived) Log("profiles: archived " + path);
            foreach (var path in cleanup.NotArchived) Log("profiles: not archived (in use) " + path);
            var name = ProfileName.Plain(profile.Gamertag);
            CancelDeleteProfile();
            if (_gate.ProfileKey == profile.ProfileId) Gate = _gate with { ProfileTag = null, ProfileKey = null }; // pick another profile
            if (_chosenLocal is { } chosen && chosen.Cloud?.ProfileId == profile.ProfileId) _chosenLocal = null;
            try { await session.RefreshMeAsync(); }
            finally { FillProfiles(); }
            Message = S.Format(cleanup.FullyArchived ? "profiles.deleted" : "profiles.deleted_not_archived", ("name", name));
        });
    }

    /// <summary>A refusal of the contract in the player's words (429 and 409 with the wait the server gave).</summary>
    internal string DeleteRefusalText(ProfileDeletionOutcome outcome)
    {
        ArgumentNullException.ThrowIfNull(outcome);
        var minutes = outcome.RetryMinutes?.ToString(CultureInfo.CurrentCulture);
        return outcome.Result switch
        {
            ProfileDeletionResult.RateLimited => minutes is null ? S["profiles.delete_rate_limited_later"] : S.Format("profiles.delete_rate_limited", ("minutes", minutes)),
            ProfileDeletionResult.InUse => minutes is null ? S["profiles.delete_in_use_now"] : S.Format("profiles.delete_in_use", ("minutes", minutes)),
            ProfileDeletionResult.DeletionPending => ErrorText(ErrorCodes.AccountsDeletionPending),
            ProfileDeletionResult.Banned => BannedText(outcome.Details),
            _ => string.Empty,
        };
    }

    private static string NewDeleteKey() => Guid.CreateVersion7().ToString();

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
