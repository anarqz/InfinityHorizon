// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;

namespace Infinity.Core.Auth;

/// <summary>
/// The platform session (ADR 0014: never the game ticket): a 15-minute access token and the
/// rotating, device-bound refresh token. Refreshes are single flight, since a refresh token used
/// twice revokes the whole family (<c>AUTH.REFRESH_REUSED</c>); <see cref="Changed"/> lets the
/// owner persist each rotated refresh token before anything else uses it.
/// </summary>
public sealed class TokenManager : IDisposable
{
    /// <summary>Refresh this long before the access token's expiry.</summary>
    public static readonly TimeSpan Margin = TimeSpan.FromSeconds(60);

    private readonly ApiClient _api;
    private readonly TimeProvider _clock;
    private readonly SemaphoreSlim _flight = new(1, 1);
    private string _access;
    private DateTimeOffset _accessExpires;
    private Secret _refresh;

    public TokenManager(ApiClient api, string accountId, string accessToken, string accessExpiresAt, Secret refreshToken, TimeProvider? clock = null)
    {
        _api = api ?? throw new ArgumentNullException(nameof(api));
        AccountId = accountId;
        _access = accessToken;
        _accessExpires = ParseInstant(accessExpiresAt);
        _refresh = refreshToken ?? throw new ArgumentNullException(nameof(refreshToken));
        _clock = clock ?? TimeProvider.System;
    }

    public string AccountId { get; }

    /// <summary>The current refresh token, for <see cref="SessionStore"/> right after sign-in.</summary>
    public Secret RefreshToken => _refresh;

    /// <summary>Raised with the new refresh token after every rotation.</summary>
    public event Action<Secret>? Changed;

    /// <summary>The refresh family was revoked or is invalid: the player must sign in again.</summary>
    public bool SignedOut { get; private set; }

    public static TokenManager From(ApiClient api, SignedIn signedIn, TimeProvider? clock = null)
    {
        ArgumentNullException.ThrowIfNull(signedIn);
        return new TokenManager(api, signedIn.AccountId, signedIn.AccessToken, signedIn.AccessExpiresAt, new Secret(signedIn.RefreshToken), clock);
    }

    /// <summary>A saved session (<see cref="SessionStore"/>): no access token yet, so the first call refreshes.</summary>
    public static TokenManager Resume(ApiClient api, string accountId, Secret refreshToken, TimeProvider? clock = null) =>
        new(api, accountId, string.Empty, "0001-01-01T00:00:00Z", refreshToken, clock);

    public static TokenManager From(ApiClient api, LauncherAccountCreated created, TimeProvider? clock = null)
    {
        ArgumentNullException.ThrowIfNull(created);
        return new TokenManager(api, created.AccountId, created.AccessToken, created.AccessExpiresAt, new Secret(created.RefreshToken), clock);
    }

    /// <summary>A valid access token, refreshing first when it is within <see cref="Margin"/> of expiry.</summary>
    public async Task<string> AccessAsync(CancellationToken cancellation = default)
    {
        await _flight.WaitAsync(cancellation).ConfigureAwait(false);
        try
        {
            // On the server's clock: accessExpiresAt is the server's instant (ApiClient.ClockOffset).
            if (_api.ServerNow(_clock) + Margin < _accessExpires)
            {
                return _access;
            }
            await RefreshLocked(cancellation).ConfigureAwait(false);
            return _access;
        }
        finally
        {
            _flight.Release();
        }
    }

    /// <summary>Runs <paramref name="call"/> with the access token; on <c>AUTH.TOKEN_EXPIRED</c> it refreshes once and retries.</summary>
    public async Task<T> WithAccessAsync<T>(Func<string, Task<T>> call, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(call);
        var access = await AccessAsync(cancellation).ConfigureAwait(false);
        try
        {
            return await call(access).ConfigureAwait(false);
        }
        catch (ApiException e) when (e.Code is ErrorCodes.AuthTokenExpired)
        {
            await _flight.WaitAsync(cancellation).ConfigureAwait(false);
            try
            {
                // Another caller may have refreshed while this one waited.
                if (_access == access)
                {
                    await RefreshLocked(cancellation).ConfigureAwait(false);
                }
                access = _access;
            }
            finally
            {
                _flight.Release();
            }
            return await call(access).ConfigureAwait(false);
        }
    }

    private async Task RefreshLocked(CancellationToken cancellation)
    {
        if (SignedOut)
        {
            throw new InvalidOperationException("signed out");
        }
        TokenPair pair;
        try
        {
            pair = await _api.RefreshAsync(new Refresh(_refresh.Expose()), cancellation).ConfigureAwait(false);
        }
        catch (ApiException e) when (e.Code is ErrorCodes.AuthRefreshReused or ErrorCodes.AuthRefreshInvalid)
        {
            SignedOut = true;
            throw;
        }
        _access = pair.AccessToken;
        _accessExpires = ParseInstant(pair.AccessExpiresAt);
        _refresh = new Secret(pair.RefreshToken);
        Changed?.Invoke(_refresh);
    }

    /// <summary>An RFC 3339 wire instant (ACCOUNTS.md §1).</summary>
    public static DateTimeOffset ParseInstant(string text) =>
        DateTimeOffset.Parse(text, CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal);

    public void Dispose() => _flight.Dispose();
}
