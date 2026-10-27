// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Shell;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    private bool _onAccount;
    private string _deletionEmail = string.Empty;
    private string _deletionConfirmation = string.Empty;
    private Deletion? AccountDeletion
    {
        get { var deletion = _session?.Deletion; InspectDeletion(ref deletion); return deletion; }
    }
    partial void InspectDeletion(ref Deletion? deletion);

    public bool ShowAccount => SignedIn && _onAccount && !ShowPickSave;
    public bool HasDeletionPending => AccountDeletion is not null;
    public bool CanRequestDeletion => CanChangeIdentity && !HasDeletionPending &&
        !string.IsNullOrWhiteSpace(DeletionEmail) &&
        _session?.Me?.Profiles.Any(p => DeletionConfirms(p, DeletionConfirmation)) == true;
    public string DeletionHint => S.Format("account.delete_hint", ("tag", _gate.ProfileTag ??
        (_session?.Me?.Profiles is { Count: > 0 } profiles ? ProfileName.Plain(profiles[0].Gamertag) : "Name")));

    /// <summary>The typed confirmation names this profile: its plain name or the server's gamertag, case-insensitively (OD-120).</summary>
    private static bool DeletionConfirms(Profile profile, string typed) =>
        typed.Trim() is { Length: > 0 } text && (string.Equals(text, profile.Gamertag, StringComparison.OrdinalIgnoreCase)
            || string.Equals(text, ProfileName.Plain(profile.Gamertag), StringComparison.OrdinalIgnoreCase));

    public string DeletionPendingText
    {
        get
        {
            var date = AccountDeletion?.PurgeAt ?? string.Empty;
            if (DateTimeOffset.TryParse(date, System.Globalization.CultureInfo.InvariantCulture, System.Globalization.DateTimeStyles.None, out var instant))
                date = instant.ToLocalTime().ToString("g", System.Globalization.CultureInfo.GetCultureInfo(S.Locale));
            return S.Format("account.delete_pending", ("date", date));
        }
    }

    public string DeletionEmail
    {
        get => _deletionEmail;
        set { _deletionEmail = value; Changed(); Changed(nameof(CanRequestDeletion)); }
    }

    public string DeletionConfirmation
    {
        get => _deletionConfirmation;
        set { _deletionConfirmation = value; Changed(); Changed(nameof(CanRequestDeletion)); }
    }

    public void OpenAccount()
    {
        if (!CanChangeIdentity) return;
        _onMods = _onSetup = _onBugReport = _onOfflineStatus = false;
        _onAccount = true; _onNews = false;
        DeletionEmail = _session?.Email ?? string.Empty;
        DeletionConfirmation = string.Empty;
        Message = null;
        Changed(string.Empty);
        _ = RefreshDiscordAsync(); // CR-S63: the link and the switches as the server has them now
    }

    private void LeaveAccount()
    {
        StopDiscordWait();
        _onAccount = false;
        _onOfflineStatus = false;
        DeletionEmail = DeletionConfirmation = string.Empty;
    }

    /// <summary>The explicit delete button is the only caller. A new password sign-in precedes every request.</summary>
    public Task RequestAccountDeletionAsync(string password)
    {
        if (!CanRequestDeletion || string.IsNullOrEmpty(password) || _session is not { } session) return Task.CompletedTask;
        var email = DeletionEmail.Trim();
        // OD-120: the player types the plain name; the server is sent its own gamertag for that profile (plain, or a legacy one).
        if (session.Me?.Profiles.FirstOrDefault(p => DeletionConfirms(p, DeletionConfirmation)) is not { } confirmed) return Task.CompletedTask;
        var confirmation = confirmed.Gamertag;
        return Native(async (idp, _, _, cancellation) =>
        {
            try
            {
                await session.RequestDeletionAsync(idp, email, password, confirmation, cancellation);
                // Once accepted, block Play even if the following read fails.
                Changed(string.Empty);
                await session.RefreshMeAsync(cancellation);
                FillProfiles();
                Message = DeletionPendingText;
            }
            catch (ApiException e) when (e.Code is ErrorCodes.AuthForbidden or ErrorCodes.AuthReauthRequired or
                ErrorCodes.AuthIdTokenInvalid or ErrorCodes.AccountsConfirmationMismatch)
            {
                Message = S[e.Code switch
                {
                    ErrorCodes.AccountsConfirmationMismatch => "account.delete_mismatch",
                    ErrorCodes.AuthForbidden => "account.delete_wrong_identity",
                    _ => "account.delete_reauth",
                }];
            }
        });
    }

    public Task CancelAccountDeletionAsync() => !HasDeletionPending || !CanChangeIdentity || _session is not { } session
        ? Task.CompletedTask : Run(async () =>
        {
            await session.CancelDeletionAsync();
            Changed(string.Empty);
            await session.RefreshMeAsync();
            FillProfiles();
            Message = S["account.delete_cancelled"];
            Changed(string.Empty);
        });
}
