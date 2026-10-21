// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using Infinity.Core.Api;
using Infinity.Core.Online;

namespace Infinity.Core.Supervisor;

/// <summary>One operation's retry schedule. It never sends requests, owns no HTTP client and extends no lease.</summary>
internal sealed class OperationBackoff(Func<double>? jitter = null)
{
    private int _failures;
    public void Success() => _failures = 0;
    public TimeSpan Failed(Exception error)
    {
        _failures = Math.Min(_failures + 1, 6);
        var seconds = Math.Min(30, Math.Pow(2, _failures - 1) + Math.Clamp((jitter ?? Random.Shared.NextDouble)(), 0, 1));
        var delay = TimeSpan.FromSeconds(seconds);
        return error is ApiException { RetryAfter: { } after } && after > delay ? after : delay;
    }
    public static bool Transient(Exception error) => error switch
    {
        ApiException api => OnlinePause.From(api) is null && ((int)api.Status >= 500 || api.Status == HttpStatusCode.TooManyRequests),
        HttpRequestException or IOException or TimeoutException => true,
        OperationCanceledException => true, // Caller cancellation is checked before retrying.
        _ => false,
    };
}
