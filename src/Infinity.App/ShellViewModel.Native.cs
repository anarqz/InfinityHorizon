// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Auth;

namespace Infinity.App;

/// <summary>OD-74: native sign-in and a separate e-mail confirmation screen.</summary>
public sealed partial class ShellViewModel
{
    private readonly NativeSignInFlow _nativeFlow = new();
    private string _email = string.Empty;
    private bool _registrationEnabled = true;
    public bool CanCreateOnlineAccount => CanStartOnlineAccount && _registrationEnabled;
    public bool ShowRegistrationDisabled => !_registrationEnabled;
    private bool _localIdentity;

    private void ObserveRegistration(ServerInfo info)
    {
        _registrationEnabled = info.RegistrationEnabled;
        _localIdentity = info.Idp?.IsLocal == true;
        Changed(string.Empty);
    }

    private void RegistrationDisabled()
    {
        _registrationEnabled = false;
        _nativeFlow.Clear();
        Message = OnlineServiceAllowsPlay ? S["native.registration_disabled"] : OnlineServiceMessage;
        Changed(string.Empty);
    }

    public string LoginEmail
    {
        get => _email;
        set
        {
            if (_email == value) return;
            _nativeFlow.Clear();
            _email = value;
            Changed(string.Empty);
        }
    }

    public bool AwaitingVerification
    {
        get { var pending = _nativeFlow.VerificationEmail is not null; InspectVerification(ref pending); return pending; }
    }
    partial void InspectVerification(ref bool pending);
    public string VerificationText => S.Format(_localIdentity ? "native.check_code" : "native.check_email", ("email", _nativeFlow.VerificationEmail ?? LoginEmail));

    public Task NativeSignInAsync(string password) => Native(async (idp, info, api, cancellation) =>
    {
        Message = S["native.signing_in"];
        var device = DeviceIdentity.Load(_stateDirectory, "windows");
        await CompleteNativeAsync(await _nativeFlow.SignInAsync(idp, api, info, device, LoginEmail.Trim(), password, cancellation), cancellation);
    });

    public Task NativeSignUpAsync(string password) => Native(async (idp, info, api, cancellation) =>
    {
        if (!info.RegistrationEnabled) { RegistrationDisabled(); return; }
        Message = S["native.creating"];
        if (idp.IsLocal)
        {
            // CR-L38: the new credential signs in right away; an address the instance mails a confirmation to (CR-S53) waits on the
            // confirmation screen, with the password kept in memory only while it is open
            var device = DeviceIdentity.Load(_stateDirectory, "windows");
            await CompleteNativeAsync(await _nativeFlow.SignUpLocalAsync(idp, api, info, device, LoginEmail.Trim(), password, cancellation), cancellation);
            if (AwaitingVerification) Message = S["native.verification_ready"];
            return;
        }
        await _nativeFlow.SignUpAsync(idp, LoginEmail.Trim(), password, cancellation);
        Message = S["native.verification_ready"];
    });

    /// <summary>CR-S53 follow-up: a local instance's mail carries a 6-digit code typed here (the link stays the alternative).</summary>
    public bool ShowConfirmationCode => _localIdentity;
    public bool ShowConfirmedButton => !_localIdentity;
    private string _confirmationCode = string.Empty;
    public string ConfirmationCode
    {
        get => _confirmationCode;
        set { if (_confirmationCode == value) return; _confirmationCode = value; Changed(nameof(CanSubmitConfirmationCode)); }
    }
    public bool CanSubmitConfirmationCode => CanStartOnlineAccount && IdentityPlatform.NormalizeCode(_confirmationCode) is not null;

    public Task SubmitConfirmationCodeAsync() => !AwaitingVerification || IdentityPlatform.NormalizeCode(ConfirmationCode) is not { } code
        ? Task.CompletedTask : Native(async (idp, info, api, cancellation) =>
        {
            Message = S["native.signing_in"];
            var device = DeviceIdentity.Load(_stateDirectory, "windows");
            try
            {
                await CompleteNativeAsync(await _nativeFlow.ConfirmCodeAsync(idp, api, info, device, code, cancellation), cancellation);
                ConfirmationCode = string.Empty;
            }
            catch (ApiException e) when (e.Code == ErrorCodes.AuthConfirmationCodeInvalid)
            {
                Message = S["native.code_invalid"]; // wrong, used, expired or replaced: retype from the newest mail, or send a new one
            }
            catch (ApiException e) when (e.Code == ErrorCodes.RequestInvalid)
            {
                // the address is already confirmed (the link, or another device): a plain sign-in and the exchange
                await CompleteNativeAsync(await _nativeFlow.ConfirmAsync(idp, api, info, device, cancellation), cancellation);
            }
            catch (ApiException e) when (e.Code is ErrorCodes.AuthRateLimited or ErrorCodes.RequestRateLimited)
            {
                Message = S["native.code_limited"];
            }
            catch (ApiException e) when (e.Code == ErrorCodes.AuthIdTokenInvalid)
            {
                _nativeFlow.Clear(); // the password changed meanwhile: sign in again
                Message = S["native.bad_credentials"];
            }
        });

    public Task ConfirmVerificationAsync() =>!AwaitingVerification ? Task.CompletedTask : Native(async (idp, info, api, cancellation) =>
    {
        Message = S["native.signing_in"];
        var device = DeviceIdentity.Load(_stateDirectory, "windows");
        await CompleteNativeAsync(await _nativeFlow.ConfirmAsync(idp, api, info, device, cancellation), cancellation);
    });

    private async Task CompleteNativeAsync((AccountSession? Session, PendingAccount? Pending) result, CancellationToken cancellation)
    {
        cancellation.ThrowIfCancellationRequested();
        if (result.Session is { } session)
        {
            try { await EnterAsync(session, cancellation); }
            catch { session.Dispose(); throw; }
        }
        else if (result.Pending is { } pending)
        {
            _pending = pending;
            Message = null;
            // OD-120: a new account's first profile is this PC's oldest local profile, whatever its mode (a Hardcore one left by 1.0.12
            // registers its name only), so the account never starts with one more profile than this PC has.
            _firstLocal = LocalProfiles.Read().Where(p => p.Game == OfflineGame.Id && p.LocalOnly)
                .OrderBy(p => p.CreatedAt, StringComparer.Ordinal).FirstOrDefault();
            if (_firstLocal is not null) NewName = _firstLocal.Name;
        }
        else Message = S["native.unverified"];
    }

    public Task NativeForgotAsync()
    {
        _nativeFlow.Clear();
        return Native(async (idp, _, _, cancellation) =>
        {
            // CR-S53: a local instance answers 202 whatever the address, like Identity Platform: the same message either way
            if (string.IsNullOrWhiteSpace(LoginEmail)) { Message = S["native.need_email"]; return; }
            await idp.SendPasswordResetAsync(LoginEmail.Trim(), cancellation);
            Message = S.Format("native.reset_sent", ("email", LoginEmail.Trim()));
        });
    }

    public Task ResendVerificationAsync() => !AwaitingVerification ? Task.CompletedTask : Native(async (idp, _, _, cancellation) =>
    {
        await _nativeFlow.ResendAsync(idp, cancellation);
        Message = S["native.verification_ready"];
    });

    public void CancelVerification()
    {
        _signIn?.Cancel();
        _nativeFlow.Clear();
        Message = null;
        Changed(string.Empty);
    }

    private async Task Native(Func<IdentityPlatform, ServerInfo, ApiClient, CancellationToken, Task> work)
    {
        if (!OnlineServiceAllowsPlay) { OpenOnlineStatus(); Message = OnlineServiceMessage; return; }
        if (_busy) return;
        Busy = true;
        using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(60));
        var epoch = _onlinePauseEpoch;
        _signIn = cancellation;
        try
        {
            Task<(ServerInfo Info, ApiClient Api)>? connection = null;
            PrepareNativeConnection(ref connection, cancellation.Token);
            var (info, api) = await (connection ?? AccountSession.ConnectAsync(Http, _config, ClientHeader, cancellation.Token)).WaitAsync(cancellation.Token);
            cancellation.Token.ThrowIfCancellationRequested();
            if (!OnlineServiceAllowsPlay || epoch != _onlinePauseEpoch) { Message = OnlineServiceMessage; return; }
            _serverReachable = true;
            ObserveRegistration(info);
            IdentityPlatform? identity = null;
            PrepareNativeIdentity(ref identity);
            await work(identity ?? IdentityPlatform.For(Http, info, api, S.Locale, _config.Custom ? _config.Server : null), info, api, cancellation.Token);
            if (!OnlineServiceAllowsPlay || epoch != _onlinePauseEpoch) Message = OnlineServiceMessage;
        }
        catch (OperationCanceledException)
        {
            _nativeFlow.Clear();
            Message = OnlineServiceAllowsPlay ? S["signin.cancelled"] : OnlineServiceMessage;
        }
        catch (IdentityPlatformException e)
        {
            if (e.Problem == IdpProblem.RegistrationDisabled) { RegistrationDisabled(); return; }
            Log($"auth: identity refused: {e.Problem}");
            if (e.Problem is IdpProblem.Credentials or IdpProblem.Disabled) _nativeFlow.Clear();
            Message = IdentityProblem(e);
        }
        catch (ApiException e)
        {
            Log($"auth: server refused: {e.Code}");
            if (Infinity.Core.Online.OnlinePause.From(e) is { } pause)
            {
                ObserveOnlinePause(pause);
                ExtendOnlineCooldown(e.RetryAfter);
                return;
            }
            if (e.Code == ErrorCodes.AccountsRegistrationDisabled) { RegistrationDisabled(); return; }
            Message = e.Code switch
            {
                ErrorCodes.AuthRateLimited or ErrorCodes.RequestRateLimited => S["native.too_many"],
                ErrorCodes.AccountsBanned => S["native.banned"],
                ErrorCodes.SharingTooManyDevices or ErrorCodes.SharingTooManyRecentDevices => S["native.too_many_devices"],
                _ => ErrorText(e.Code),
            };
        }
        catch (TrustChangedException e)
        {
            _nativeFlow.Clear();
            ObserveTrustChange(e);
        }
        catch (OfficialIdentityRefusedException)
        {
            // OD-122 follow-up: an official-account sign-in would hand this server that account
            _nativeFlow.Clear();
            Log("auth: official-account sign-in refused for this custom server");
            Message = S["server.official_identity_refused"];
        }
        catch (InvalidDataException e) when (_config.Custom)
        {
            // CR-L39: a custom server's discovery the launcher does not accept (official origins, its realm, no keys)
            Log($"auth: custom server discovery refused: {e.GetType().Name}");
            Message = S["server.invalid_discovery"];
        }
        catch (Exception e) when (e is HttpRequestException or IOException or JsonException)
        {
            Log($"auth: unreachable: {e.GetType().Name}");
            _serverReachable = false;
            Message = S["native.unreachable"];
        }
        finally
        {
            if (_onlinePause is not null) Message = OnlineServiceMessage;
            _signIn = null;
            Busy = false;
            Changed(string.Empty);
        }
    }

    partial void PrepareNativeConnection(ref Task<(ServerInfo Info, ApiClient Api)>? connection, CancellationToken cancellation);
    partial void PrepareNativeIdentity(ref IdentityPlatform? identity);

    private string IdentityProblem(IdentityPlatformException e) => e.Problem switch
    {
        // CR-S53: an account moved without a password (Google-only) sets one through "Forgot password?"
        IdpProblem.Credentials => S[_localIdentity ? "native.local_bad_credentials" : "native.bad_credentials"],
        IdpProblem.TooManyAttempts => S["native.too_many"],
        IdpProblem.Disabled => S["native.disabled"],
        IdpProblem.EmailExists => S[_localIdentity ? "native.local_email_exists" : "native.email_exists"],
        IdpProblem.WeakPassword => S[_localIdentity ? "native.local_weak_password" : "native.weak_password"],
        IdpProblem.InvalidEmail => S["native.invalid_email"],
        _ => S["native.failed"],
    };
}
