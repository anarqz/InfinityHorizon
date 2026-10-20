// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;

namespace Infinity.Core.Auth;

/// <summary>
/// OD-74: credentials live only while the verification screen is open. Confirmation and resend
/// perform a fresh password sign-in, so neither needs an expired Identity Platform token.
/// Clear invalidates in-flight results as well as erasing the retained password buffer.
/// </summary>
public sealed class NativeSignInFlow : IDisposable
{
    private readonly object _sync = new();
    private char[]? _password;
    private string? _email;
    private long _generation;

    public string? VerificationEmail { get { lock (_sync) return _email; } }

    public void Clear() => Reset();

    private long Reset()
    {
        lock (_sync)
        {
            _generation++;
            if (_password is not null) Array.Clear(_password);
            _password = null;
            _email = null;
            return _generation;
        }
    }

    public async Task SignUpAsync(IdentityPlatform idp, string email, string password, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(idp);
        var generation = Reset();
        await idp.SignUpAsync(email, password, cancellation).ConfigureAwait(false);
        Remember(generation, email, password, cancellation);
    }

    public Task<(AccountSession? Session, PendingAccount? Pending)> SignInAsync(
        IdentityPlatform idp, ApiClient api, ServerInfo info, Device device, string email, string password, CancellationToken cancellation = default)
    {
        return ExchangeAsync(idp, api, info, device, email, password, Reset(), cancellation);
    }

    public Task<(AccountSession? Session, PendingAccount? Pending)> ConfirmAsync(
        IdentityPlatform idp, ApiClient api, ServerInfo info, Device device, CancellationToken cancellation = default)
    {
        var (email, password, generation) = Credentials();
        return ExchangeAsync(idp, api, info, device, email, password, generation, cancellation);
    }

    public async Task ResendAsync(IdentityPlatform idp, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(idp);
        var (email, password, generation) = Credentials();
        var idToken = await idp.SignInAsync(email, password, cancellation).ConfigureAwait(false);
        Check(generation, cancellation);
        await idp.SendVerificationAsync(idToken, cancellation).ConfigureAwait(false);
        Check(generation, cancellation);
    }

    private async Task<(AccountSession? Session, PendingAccount? Pending)> ExchangeAsync(
        IdentityPlatform idp, ApiClient api, ServerInfo info, Device device, string email, string password, long generation, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(idp);
        var idToken = await idp.SignInAsync(email, password, cancellation).ConfigureAwait(false);
        Check(generation, cancellation);
        try
        {
            var result = await AccountSession.SignInWithIdTokenAsync(api, info, device, idToken, cancellation, email).ConfigureAwait(false);
            try
            {
                lock (_sync)
                {
                    Check(generation, cancellation);
                    Reset();
                }
            }
            catch
            {
                result.Session?.Dispose();
                throw;
            }
            return result;
        }
        catch (ApiException e) when (e.Code == ErrorCodes.AuthEmailUnverified)
        {
            Remember(generation, email, password, cancellation);
            return (null, null);
        }
    }

    private void Check(long generation, CancellationToken cancellation)
    {
        cancellation.ThrowIfCancellationRequested();
        lock (_sync)
        {
            if (_generation != generation) throw new OperationCanceledException("Sign-in screen closed.", cancellation);
        }
    }

    private void Remember(long generation, string email, string password, CancellationToken cancellation)
    {
        lock (_sync)
        {
            Check(generation, cancellation);
            if (_password is not null) Array.Clear(_password);
            _email = email;
            _password = password.ToCharArray();
        }
    }

    private (string Email, string Password, long Generation) Credentials()
    {
        lock (_sync)
        {
            if (_email is null || _password is null) throw new InvalidOperationException("No pending e-mail verification.");
            return (_email, new string(_password), _generation);
        }
    }

    public void Dispose() => Clear();
}
