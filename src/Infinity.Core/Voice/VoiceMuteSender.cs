// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using Infinity.Core.Api;

namespace Infinity.Core.Voice;

/// <summary>
/// CR-S68 (CORE_API.md §4.4): reports this profile's voice mutes and unmutes to the server, <c>POST /launcher/v1/voice/mutes</c>.
/// The route is idempotent by state, so each change is retried until 202 with a growing delay (2 s doubling to 5 minutes), a 429
/// waits its <c>Retry-After</c>, a 404 <c>VOICE.NOT_SHARED</c> stops (no shared world in the last 30 minutes: nothing to count) and a
/// 400 is logged and dropped. A newer change for the same player replaces a queued one. Logs never carry the XUID.
/// </summary>
public sealed class VoiceMuteSender : IDisposable
{
    /// <summary>At most this many players wait to be reported; the oldest is dropped past it (logged).</summary>
    public const int MaxQueued = 64;
    public static readonly TimeSpan FirstRetry = TimeSpan.FromSeconds(2);
    public static readonly TimeSpan LongestRetry = TimeSpan.FromMinutes(5);
    /// <summary>A change still refused after this many attempts (about two hours of retries) is dropped, logged.</summary>
    public const int MaxAttempts = 30;

    private sealed record Pending(VoiceMuteReport Mute, int Attempts, DateTimeOffset Due);

    private readonly Func<VoiceMuteReport, CancellationToken, Task> _post;
    private readonly Action<string> _log;
    private readonly TimeProvider _clock;
    private readonly object _lock = new();
    private readonly List<Pending> _queue = [];
    private readonly CancellationTokenSource _stop = new();
    private Task _pump = Task.CompletedTask;
    private bool _pumping;

    /// <param name="post">The call (the launcher's access token inside); throws <see cref="ApiException"/> for an error answer.</param>
    public VoiceMuteSender(Func<VoiceMuteReport, CancellationToken, Task> post, Action<string>? log = null, TimeProvider? clock = null)
    {
        _post = post ?? throw new ArgumentNullException(nameof(post));
        _log = log ?? (_ => { });
        _clock = clock ?? TimeProvider.System;
    }

    /// <summary>The server took a <c>muted</c> for this XUID (202): its history entry can be marked sent.</summary>
    public Action<string>? Accepted { get; init; }

    /// <summary>The players still waiting to be reported (tests and diagnostics).</summary>
    public int QueuedCount { get { lock (_lock) return _queue.Count; } }

    /// <summary>Queues a change (never blocks; safe under another lock).</summary>
    public void Report(string xuid, bool muted, string worldId)
    {
        ArgumentException.ThrowIfNullOrEmpty(xuid);
        ArgumentException.ThrowIfNullOrEmpty(worldId);
        var mute = new VoiceMuteReport { Xuid = xuid, Action = muted ? VoiceMuteActions.Muted : VoiceMuteActions.Unmuted, WorldId = worldId };
        lock (_lock)
        {
            if (_stop.IsCancellationRequested) return;
            _queue.RemoveAll(p => p.Mute.Xuid == xuid);
            if (_queue.Count >= MaxQueued) { _queue.RemoveAt(0); _log("voice mute: the report queue is full; the oldest change was dropped"); }
            _queue.Add(new(mute, 0, _clock.GetUtcNow()));
            if (_pumping) return;
            _pumping = true;
            _pump = Task.Run(PumpAsync);
        }
    }

    /// <summary>Completes when nothing is queued or due any more (tests).</summary>
    public Task Drained { get { lock (_lock) return _pump; } }

    /// <summary>Stops: queued changes are not sent (the history keeps <c>Sent: false</c>).</summary>
    public void Stop()
    {
        lock (_lock) { if (!_stop.IsCancellationRequested) _stop.Cancel(); _queue.Clear(); }
    }

    /// <summary><see cref="Stop"/>; the token source is left to the pump, which may still be observing it.</summary>
    public void Dispose() => Stop();

    private async Task PumpAsync()
    {
        var token = _stop.Token;
        try
        {
            while (!token.IsCancellationRequested)
            {
                Pending? next;
                TimeSpan wait;
                lock (_lock)
                {
                    if (_queue.Count == 0) { _pumping = false; return; }
                    next = _queue.MinBy(p => p.Due)!;
                    wait = next.Due - _clock.GetUtcNow();
                }
                // at most a second at a time: a newer change, or a clock that moved, is seen soon
                if (wait > TimeSpan.Zero) { await Task.Delay(wait < TimeSpan.FromSeconds(1) ? wait : TimeSpan.FromSeconds(1), token).ConfigureAwait(false); continue; }
                var outcome = await SendAsync(next.Mute, token).ConfigureAwait(false);
                lock (_lock)
                {
                    // a newer change for this player replaced it meanwhile: that one is sent on its own
                    var index = _queue.IndexOf(next);
                    if (index < 0) continue;
                    if (outcome.Done) { _queue.RemoveAt(index); continue; }
                    var attempts = next.Attempts + 1;
                    if (attempts >= MaxAttempts) { _queue.RemoveAt(index); _log($"voice mute: {next.Mute.Action} given up after {attempts} attempts"); continue; }
                    var backoff = outcome.RetryAfter ?? Backoff(attempts);
                    _queue[index] = next with { Attempts = attempts, Due = _clock.GetUtcNow() + backoff };
                }
            }
        }
        catch (OperationCanceledException) when (token.IsCancellationRequested) { }
        finally { lock (_lock) if (token.IsCancellationRequested) _pumping = false; }
    }

    internal static TimeSpan Backoff(int attempts)
    {
        var seconds = FirstRetry.TotalSeconds * Math.Pow(2, Math.Min(attempts - 1, 16));
        return seconds >= LongestRetry.TotalSeconds ? LongestRetry : TimeSpan.FromSeconds(seconds);
    }

    private async Task<(bool Done, TimeSpan? RetryAfter)> SendAsync(VoiceMuteReport mute, CancellationToken token)
    {
        try
        {
            await _post(mute, token).ConfigureAwait(false);
            _log($"voice mute: {mute.Action} accepted");
            try { if (mute.Action == VoiceMuteActions.Muted) Accepted?.Invoke(mute.Xuid); }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException) { _log("voice mute: the history could not be marked sent"); }
            return (true, null);
        }
        catch (ApiException error) when (error.Code == ErrorCodes.VoiceNotShared || error.Status == HttpStatusCode.NotFound)
        {
            _log($"voice mute: {mute.Action} not counted (no shared world); not retried");
            return (true, null);
        }
        catch (ApiException error) when (error.Status == HttpStatusCode.BadRequest)
        {
            _log($"voice mute: {mute.Action} refused ({error.Code}); dropped");
            return (true, null);
        }
        catch (ApiException error) when (error.Status == HttpStatusCode.TooManyRequests)
        {
            _log($"voice mute: {mute.Action} rate limited ({error.Code}); retrying later");
            return (false, error.RetryAfter is { } after && after > TimeSpan.Zero ? (after > LongestRetry * 12 ? LongestRetry * 12 : after) : null);
        }
        catch (Exception error) when (error is ApiException or HttpRequestException or IOException or TimeoutException
            || error is OperationCanceledException && !token.IsCancellationRequested)
        {
            _log($"voice mute: {mute.Action} not sent ({(error as ApiException)?.Code ?? error.GetType().Name}); retrying");
            return (false, null);
        }
    }
}
