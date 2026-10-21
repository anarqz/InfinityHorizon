// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;
using System.Text.Json;
using Infinity.Core.Api;

namespace Infinity.Core.Online;

/// <summary>Local terminal activity decision. It is not a wire DTO or a promise of reopening.</summary>
public sealed record OnlinePause(string Code, string Reason, DateTimeOffset? ClosedAt, ulong? Revision)
{
    public static OnlinePause? From(ApiException error)
    {
        ArgumentNullException.ThrowIfNull(error);
        return From(error.Code, error.Status, error.Details);
    }

    public static OnlinePause? From(string code, HttpStatusCode status, JsonElement? details = null)
    {
        if (status != HttpStatusCode.ServiceUnavailable || code is not (ErrorCodes.ServiceBudgetClosed or ErrorCodes.ServiceOnlineClosed)) return null;
        var reason = code == ErrorCodes.ServiceBudgetClosed ? "budget" : "operator";
        // The typed closed code still stops activity when optional diagnostics are unreadable.
        try
        {
            if (details?.Deserialize<OnlineStatus>(ApiClient.Json) is { } value)
            {
                OnlineStatusRules.Validate(value);
                if (value.State == "closed" && value.Reason == reason) return FromStatus(value);
            }
        }
        catch (Exception error) when (error is JsonException or InvalidDataException or ArgumentException) { }
        return new(code, reason, null, null);
    }

    public static OnlinePause FromStatus(OnlineStatus status)
    {
        OnlineStatusRules.Validate(status);
        if (status.State != "closed") throw new ArgumentException("The online status is not closed.", nameof(status));
        return new(status.Reason == "budget" ? ErrorCodes.ServiceBudgetClosed : ErrorCodes.ServiceOnlineClosed,
            status.Reason!, OnlineStatusRules.Instant(status.ClosedAt!), status.Revision);
    }
}

public static class OnlineStatusRules
{
    public static void Validate(OnlineStatus status)
    {
        ArgumentNullException.ThrowIfNull(status);
        if (status.State == "open" && status.Reason is null && status.ClosedAt is null && status.RetryAfterSeconds == 5) return;
        if (status.State == "closed" && status.Reason is "budget" or "operator" && status.RetryAfterSeconds == 300
            && DateTimeOffset.TryParseExact(status.ClosedAt, "yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'", CultureInfo.InvariantCulture,
                DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out _)) return;
        throw new InvalidDataException("Invalid online availability snapshot.");
    }

    internal static DateTimeOffset Instant(string value) => DateTimeOffset.ParseExact(value, "yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'",
        CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal);
}

/// <summary>One online session's irreversible latch. A later open snapshot cannot resume its tokens or world operations.</summary>
public sealed class OnlineActivity : IDisposable
{
    private readonly object _startGate = new();
    private readonly CancellationTokenSource _stop = new();
    private readonly CancellationToken _cancellation;
    private readonly TaskCompletionSource<OnlinePause> _paused = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private OnlinePause? _pause;
    private int _disposed;
    public OnlineActivity() => _cancellation = _stop.Token;
    public bool IsPaused => Volatile.Read(ref _pause) is not null;
    public CancellationToken Cancellation => _cancellation;
    public Task<OnlinePause> Paused => _paused.Task;
    public bool TryPause(ApiException error)
    {
        var pause = OnlinePause.From(error);
        return pause is not null && TryPause(pause);
    }
    public bool TryPause(string code, HttpStatusCode status, JsonElement? details = null)
    {
        var pause = OnlinePause.From(code, status, details);
        return pause is not null && TryPause(pause);
    }
    public bool TryPause(OnlinePause pause)
    {
        ArgumentNullException.ThrowIfNull(pause);
        lock (_startGate)
        {
            if (Volatile.Read(ref _disposed) != 0 || _pause is not null) return false;
            Volatile.Write(ref _pause, pause);
        }
        _paused.TrySetResult(pause);
        // Cancellation is cooperative; callbacks must not turn a closed decision into resumed activity.
        try { _stop.Cancel(); } catch (Exception error) when (error is AggregateException or ObjectDisposedException) { }
        return true;
    }
    /// <summary>Orders a synchronous start against the terminal latch; already dispatched OS work cannot be recalled.</summary>
    internal bool TryStart<T>(Func<T> start, out T value)
    {
        lock (_startGate)
        {
            if (IsPaused || Volatile.Read(ref _disposed) != 0) { value = default!; return false; }
            value = start(); return true;
        }
    }
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        try { _stop.Cancel(); } catch (AggregateException) { }
        _stop.Dispose();
    }
}
