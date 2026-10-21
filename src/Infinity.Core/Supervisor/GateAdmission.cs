// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Online;

namespace Infinity.Core.Supervisor;

/// <summary>Cancellable admission before any game starts (GATE_API §2, §5). No client-side queue ordering.</summary>
public sealed class GateAdmission(IGate gate, Action<SupervisorEvent> emit, TimeProvider? clock = null)
{
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private static readonly TimeSpan QueueLife = TimeSpan.FromSeconds(90);

    public async Task<(string SessionId, GateActive Active)?> WaitAsync(
        Func<string, PlaySessionCreate> request, CancellationToken cancellation,
        Func<string, CancellationToken, Task>? prepare = null, string? initialSessionId = null)
    {
        ArgumentNullException.ThrowIfNull(request);
        string? openSession = null;
        ulong? fence = null;
        try
        {
            while (true)
            {
                cancellation.ThrowIfCancellationRequested();
                var id = initialSessionId ?? Guid.CreateVersion7().ToString();
                initialSessionId = null;
                GateSession session;
                var lastConfirmed = _clock.GetTimestamp();
                emit(new SupervisorEvent.RequestingAdmission());
                try
                {
                    // CR-L29 binds a verified scan to this exact future session before create.
                    // No gate session exists yet if preparing fails or is cancelled.
                    if (prepare is not null) await prepare(id, cancellation).ConfigureAwait(false);
                    // Until a definitive refusal arrives, cancellation must clean up even a lost create response.
                    openSession = id;
                    var create = request(id);
                    session = await AttemptAsync(() => gate.CreateAsync(create, cancellation), id, lastConfirmed, cancellation).ConfigureAwait(false);
                    fence = (session as GateActive)?.Lease?.FenceToken;
                    if (_clock.GetElapsedTime(lastConfirmed) >= QueueLife) throw new AdmissionExpiredException();
                    lastConfirmed = _clock.GetTimestamp();
                }
                catch (ApiException e) when (Retryable(e))
                {
                    openSession = null; // no place exists after a definitive admission refusal
                    await RetryAsync(e, admission: true, null, cancellation).ConfigureAwait(false);
                    continue; // a new admission gets a new idempotency key
                }
                catch (ApiException error) when (OnlinePause.From(error) is null)
                {
                    openSession = null;
                    throw;
                }
                while (session is GateQueued queued)
                {
                    emit(new SupervisorEvent.Queued(queued.Queue, queued.Pool));
                    await DelayWithinAsync(TimeSpan.FromSeconds(Math.Clamp(queued.Queue.RetryAfterSeconds, 5, 30)), lastConfirmed, cancellation).ConfigureAwait(false);
                    while (true)
                    {
                        try
                        {
                            session = await AttemptAsync(() => gate.PollAsync(id, cancellation), id, lastConfirmed, cancellation).ConfigureAwait(false);
                            fence = (session as GateActive)?.Lease?.FenceToken;
                            if (_clock.GetElapsedTime(lastConfirmed) >= QueueLife) throw new AdmissionExpiredException();
                            lastConfirmed = _clock.GetTimestamp();
                            break;
                        }
                        catch (ApiException e) when (e.Code == ErrorCodes.SessionsEnded)
                        {
                            openSession = null;
                            // Only a bounded reason code is logged, never arbitrary server text or details.
                            var reason = Detail(e, "reason");
                            reason = reason is { Length: <= 40 } && reason.All(c => char.IsAsciiLetterOrDigit(c) || c == '_') ? reason : "unknown";
                            emit(new SupervisorEvent.Note("queue ended: " + reason));
                            emit(new SupervisorEvent.Ended("queue_ended", true));
                            return null;
                        }
                        catch (ApiException e) when (Retryable(e))
                        {
                            await RetryAsync(e, admission: false, lastConfirmed, cancellation).ConfigureAwait(false);
                        }
                    }
                }
                var active = (GateActive)session;
                fence = active.Lease?.FenceToken;
                cancellation.ThrowIfCancellationRequested(); // admission won the race with Leave: release its lease too
                return (id, active);
            }
        }
        catch (AdmissionExpiredException)
        {
            var left = openSession is null || await LeaveAsync(openSession, fence).ConfigureAwait(false);
            emit(new SupervisorEvent.Ended("queue_timeout", left));
            return null;
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
        {
            var left = openSession is null || await LeaveAsync(openSession, fence).ConfigureAwait(false);
            emit(new SupervisorEvent.Ended("queue_cancelled", left));
            return null;
        }
        catch
        {
            if (openSession is not null)
            {
                var left = await LeaveAsync(openSession, fence).ConfigureAwait(false);
                if (!left)
                    emit(new SupervisorEvent.Note("queue cleanup pending; server expiry will release the place"));
            }
            throw;
        }
    }

    private async Task<GateSession> AttemptAsync(Func<Task<GateSession>> request, string id, long lastConfirmed, CancellationToken cancellation)
    {
        var retry = new OperationBackoff();
        while (true)
        {
            cancellation.ThrowIfCancellationRequested();
            if (_clock.GetElapsedTime(lastConfirmed) >= QueueLife) throw new AdmissionExpiredException();
            try
            {
                var pending = request();
                try { return await pending.WaitAsync(cancellation).ConfigureAwait(false); }
                catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
                {
                    _ = CleanupLateAsync(pending, id);
                    throw;
                }
            }
            catch (ApiException error) when (Retryable(error)) { throw; }
            catch (Exception error) when (OperationBackoff.Transient(error) && !cancellation.IsCancellationRequested)
            {
                var delay = retry.Failed(error);
                emit(new SupervisorEvent.AdmissionRetry(error is ApiException api ? api.Code : "network", delay, null));
                await DelayWithinAsync(delay, lastConfirmed, cancellation).ConfigureAwait(false);
            }
        }
    }

    private async Task CleanupLateAsync(Task<GateSession> pending, string id)
    {
        try
        {
            var result = await pending.ConfigureAwait(false);
            await LeaveAsync(id, (result as GateActive)?.Lease?.FenceToken).ConfigureAwait(false);
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or IOException or OperationCanceledException or TimeoutException) { }
    }

    private async Task DelayWithinAsync(TimeSpan delay, long lastConfirmed, CancellationToken cancellation)
    {
        var left = QueueLife - _clock.GetElapsedTime(lastConfirmed);
        if (left <= TimeSpan.Zero) throw new AdmissionExpiredException();
        await Task.Delay(delay < left ? delay : left, _clock, cancellation).ConfigureAwait(false);
        if (_clock.GetElapsedTime(lastConfirmed) >= QueueLife) throw new AdmissionExpiredException();
    }

    private sealed class AdmissionExpiredException : Exception;

    private async Task RetryAsync(ApiException error, bool admission, long? lastConfirmed, CancellationToken cancellation)
    {
        // At most two new admissions/minute; never shorten the server's Retry-After (including HTTP-date).
        var minimum = TimeSpan.FromSeconds(admission ? 30 : 5);
        var delay = error.RetryAfter is { } after && after > minimum ? after : minimum;
        delay += TimeSpan.FromMilliseconds(Random.Shared.Next(250, 1251));
        emit(new SupervisorEvent.AdmissionRetry(error.Code, delay, Detail(error, "until")));
        if (lastConfirmed is { } confirmed) await DelayWithinAsync(delay, confirmed, cancellation).ConfigureAwait(false);
        else await Task.Delay(delay, _clock, cancellation).ConfigureAwait(false);
    }

    private static bool Retryable(ApiException e) =>
        OnlinePause.From(e) is null &&
        ((e.Status == HttpStatusCode.ServiceUnavailable && e.Code is ErrorCodes.PoolFull or ErrorCodes.ServiceMaintenance) ||
        (e.Status == HttpStatusCode.TooManyRequests && e.Code == ErrorCodes.RequestRateLimited));

    private static string? Detail(ApiException e, string key) =>
        e.Details is { ValueKind: JsonValueKind.Object } d && d.TryGetProperty(key, out var v) && v.ValueKind == JsonValueKind.String
            ? v.GetString() : null;

    private async Task<bool> LeaveAsync(string id, ulong? fence)
    {
        using var bound = new CancellationTokenSource(TimeSpan.FromSeconds(10));
        try
        {
            await gate.LeaveAsync(id, "player", fence, bound.Token).ConfigureAwait(false);
            return true;
        }
        catch (Exception e) when (e is ApiException or HttpRequestException or IOException or OperationCanceledException)
        {
            return false;
        }
    }
}
