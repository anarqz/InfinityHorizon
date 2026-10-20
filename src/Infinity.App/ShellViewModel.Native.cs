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
    public string VerificationText => S.Format("native.check_email", ("email", _nativeFlow.VerificationEmail ?? LoginEmail));

    public Task NativeSignInAsync(string password) => Native(async (idp, info, api, cancellation) =>
    {
        Message = S["native.signing_in"];
        var device = DeviceIdentity.Load(_stateDirectory, "windows");
        await CompleteNativeAsync(await _nativeFlow.SignInAsync(idp, api, info, device, LoginEmail.Trim(), password, cancellation), cancellation);
    });

    public Task NativeSignUpAsync(string password) => Native(async (idp, _, _, cancellation) =>
    {
        Message = S["native.creating"];
        await _nativeFlow.SignUpAsync(idp, LoginEmail.Trim(), password, cancellation);
        Message = S["native.verification_ready"];
    });

    public Task ConfirmVerificationAsync() => !AwaitingVerification ? Task.CompletedTask : Native(async (idp, info, api, cancellation) =>
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
        }
        else Message = S["native.unverified"];
    }

    public Task NativeForgotAsync()
    {
        _nativeFlow.Clear();
        return Native(async (idp, _, _, cancellation) =>
        {
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
        if (_busy) return;
        Busy = true;
        using var cancellation = new CancellationTokenSource(TimeSpan.FromSeconds(60));
        _signIn = cancellation;
        try
        {
            var (info, api) = await AccountSession.ConnectAsync(Http, _config, ClientHeader, cancellation.Token);
            _serverReachable = true;
            await work(new IdentityPlatform(Http, info.Idp.ApiKey, info.Idp.EmulatorUrl, S.Locale), info, api, cancellation.Token);
        }
        catch (OperationCanceledException)
        {
            _nativeFlow.Clear();
            Message = S["signin.cancelled"];
        }
        catch (IdentityPlatformException e)
        {
            Log($"auth: identity refused: {e.Problem}");
            if (e.Problem is IdpProblem.Credentials or IdpProblem.Disabled) _nativeFlow.Clear();
            Message = IdentityProblem(e);
        }
        catch (ApiException e)
        {
            Log($"auth: server refused: {e.Code}");
            Message = e.Code switch
            {
                ErrorCodes.AuthRateLimited or ErrorCodes.RequestRateLimited => S["native.too_many"],
                ErrorCodes.AccountsBanned => S["native.banned"],
                ErrorCodes.SharingTooManyDevices or ErrorCodes.SharingTooManyRecentDevices => S["native.too_many_devices"],
                _ => S["native.failed"] + " (" + e.Code + ")",
            };
        }
        catch (Exception e) when (e is HttpRequestException or IOException or JsonException)
        {
            Log($"auth: unreachable: {e.GetType().Name}");
            _serverReachable = false;
            Message = S["native.unreachable"];
        }
        finally
        {
            _signIn = null;
            Busy = false;
            Changed(string.Empty);
        }
    }

    private string IdentityProblem(IdentityPlatformException e) => e.Problem switch
    {
        IdpProblem.Credentials => S["native.bad_credentials"],
        IdpProblem.TooManyAttempts => S["native.too_many"],
        IdpProblem.Disabled => S["native.disabled"],
        IdpProblem.EmailExists => S["native.email_exists"],
        IdpProblem.WeakPassword => S["native.weak_password"],
        IdpProblem.InvalidEmail => S["native.invalid_email"],
        _ => S["native.failed"],
    };
}
