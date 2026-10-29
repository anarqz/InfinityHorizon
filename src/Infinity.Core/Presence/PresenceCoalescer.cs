// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json.Nodes;

namespace Infinity.Core.Presence;

/// <summary>What <see cref="PresenceCoalescer.Next"/> asks the presence loop to do now.</summary>
public enum PresenceStepKind
{
    /// <summary>Nothing to do until the wanted card changes.</summary>
    Idle,

    /// <summary>Nothing until <see cref="PresenceStep.Until"/> (the 15 s send window or the 30 s reconnect), unless stopped.</summary>
    Wait,

    /// <summary>Open the connection to Discord.</summary>
    Connect,

    /// <summary><c>SET_ACTIVITY</c> with <see cref="PresenceStep.Activity"/> (null clears).</summary>
    Send,
}

public readonly record struct PresenceStep(PresenceStepKind Kind, string? Activity = null, DateTimeOffset Until = default);

/// <summary>
/// The presence's send rules (DISCORD_PRESENCE.md §3), without I/O so they test on fixed times: at most one
/// <c>SET_ACTIVITY</c> every 15 s (Discord allows 5 per 20 s), the last wanted card always wins, an unchanged card is
/// never re-sent, and a missing Discord is retried every 30 s, silently. Nothing connects while nothing is wanted.
/// A new connection starts with no card shown. Thread-safe.
/// </summary>
public sealed class PresenceCoalescer
{
    public static readonly TimeSpan SendEvery = TimeSpan.FromSeconds(15);
    public static readonly TimeSpan ReconnectEvery = TimeSpan.FromSeconds(30);

    private readonly object _sync = new();
    private string? _wanted;
    private string? _shown;
    private bool _connected;
    private DateTimeOffset _lastSend = DateTimeOffset.MinValue;
    private DateTimeOffset _nextConnect = DateTimeOffset.MinValue;

    /// <summary>The card the launcher wants now (its JSON), or null to clear it.</summary>
    public void Want(string? activity)
    {
        lock (_sync) _wanted = activity;
    }

    public string? Wanted
    {
        get { lock (_sync) return _wanted; }
    }

    /// <summary>The card Discord shows as far as this launcher knows (null: none, or not connected).</summary>
    public string? Shown
    {
        get { lock (_sync) return _shown; }
    }

    public bool Connected
    {
        get { lock (_sync) return _connected; }
    }

    public PresenceStep Next(DateTimeOffset now)
    {
        lock (_sync)
        {
            if (!_connected)
            {
                if (_wanted is null) return new(PresenceStepKind.Idle);
                return now < _nextConnect ? new(PresenceStepKind.Wait, Until: _nextConnect) : new(PresenceStepKind.Connect);
            }
            if (string.Equals(_wanted, _shown, StringComparison.Ordinal)) return new(PresenceStepKind.Idle);
            var due = _lastSend == DateTimeOffset.MinValue ? now : _lastSend + SendEvery;
            return now < due ? new(PresenceStepKind.Wait, Until: due) : new(PresenceStepKind.Send, _wanted);
        }
    }

    public void ConnectSucceeded()
    {
        lock (_sync) { _connected = true; _shown = null; }
    }

    public void ConnectFailed(DateTimeOffset now)
    {
        lock (_sync) { _connected = false; _shown = null; _nextConnect = now + ReconnectEvery; }
    }

    public void SendSucceeded(string? activity, DateTimeOffset now)
    {
        lock (_sync) { _shown = activity; _lastSend = now; }
    }

    /// <summary>The pipe broke or Discord refused: drop the connection, reconnect after 30 s.</summary>
    public void SendFailed(DateTimeOffset now) => ConnectFailed(now);
}

/// <summary>
/// The presence loop: feeds <see cref="PresenceCoalescer"/> steps to a Discord connection off the UI thread.
/// <see cref="Want"/> never blocks; <see cref="StopAsync"/> clears the card at once (it is the last send, so it is not
/// held to the 15 s window) and closes the pipe, bounded. Failures are only logged.
/// </summary>
public sealed class DiscordPresenceService : IAsyncDisposable
{
    private readonly Func<CancellationToken, Task<IDiscordConnection?>> _connect;
    private readonly TimeProvider _clock;
    private readonly Action<string> _log;
    private readonly PresenceCoalescer _rules = new();
    private readonly SemaphoreSlim _signal = new(0, int.MaxValue);
    private readonly CancellationTokenSource _stop = new();
    private readonly Task _loop;
    private IDiscordConnection? _connection;
    private int _stopped;

    public DiscordPresenceService(Func<CancellationToken, Task<IDiscordConnection?>> connect, TimeProvider? clock = null, Action<string>? log = null)
    {
        _connect = connect ?? throw new ArgumentNullException(nameof(connect));
        _clock = clock ?? TimeProvider.System;
        _log = log ?? (_ => { });
        _loop = Task.Run(RunAsync);
    }

    /// <summary>The official client over Discord's pipes (the "Infinity" application).</summary>
    public static DiscordPresenceService ForDiscord(Action<string>? log = null) =>
        new(async ct => await DiscordIpcClient.ConnectAsync(DiscordIpcClient.InfinityClientId, ct).ConfigureAwait(false), log: log);

    public PresenceCoalescer Rules => _rules;

    /// <summary>The card to show (null clears it); the latest call wins.</summary>
    public void Want(JsonObject? activity) => Want(activity?.ToJsonString());

    public void Want(string? activity)
    {
        if (Volatile.Read(ref _stopped) != 0) return;
        _rules.Want(activity);
        _signal.Release();
    }

    private async Task RunAsync()
    {
        var token = _stop.Token;
        while (!token.IsCancellationRequested)
        {
            var step = _rules.Next(_clock.GetUtcNow());
            try
            {
                switch (step.Kind)
                {
                    case PresenceStepKind.Idle:
                        await _signal.WaitAsync(token).ConfigureAwait(false);
                        break;
                    case PresenceStepKind.Wait:
                        var wait = step.Until - _clock.GetUtcNow();
                        if (wait > TimeSpan.Zero) await Task.Delay(wait, _clock, token).ConfigureAwait(false);
                        break;
                    case PresenceStepKind.Connect:
                        var connection = await _connect(token).ConfigureAwait(false);
                        if (connection is null) { _rules.ConnectFailed(_clock.GetUtcNow()); break; }
                        _connection = connection;
                        _rules.ConnectSucceeded();
                        _log("discord: connected");
                        break;
                    case PresenceStepKind.Send:
                        await _connection!.SetActivityAsync(step.Activity is null ? null : JsonNode.Parse(step.Activity)!.AsObject(), token).ConfigureAwait(false);
                        _rules.SendSucceeded(step.Activity, _clock.GetUtcNow());
                        break;
                }
            }
            catch (OperationCanceledException) when (token.IsCancellationRequested)
            {
                break;
            }
            catch (Exception error) when (error is IOException or System.Text.Json.JsonException or OperationCanceledException or ObjectDisposedException
                or InvalidOperationException)
            {
                _log("discord: connection dropped (" + error.GetType().Name + ")");
                await DropAsync().ConfigureAwait(false);
                _rules.SendFailed(_clock.GetUtcNow());
            }
        }
    }

    private async Task DropAsync()
    {
        var connection = Interlocked.Exchange(ref _connection, null);
        if (connection is null) return;
        try { await connection.DisposeAsync().ConfigureAwait(false); }
        catch (IOException) { }
    }

    /// <summary>Clears the card (when one is shown) and closes the pipe; at most <paramref name="bound"/>.</summary>
    public async Task StopAsync(TimeSpan bound)
    {
        if (Interlocked.Exchange(ref _stopped, 1) != 0) return;
        await _stop.CancelAsync().ConfigureAwait(false);
        try { await _loop.WaitAsync(bound).ConfigureAwait(false); }
        catch (TimeoutException) { }
        if (_connection is { } connection && _rules.Shown is not null)
        {
            using var limit = new CancellationTokenSource(bound);
            try { await connection.SetActivityAsync(null, limit.Token).ConfigureAwait(false); }
            catch (Exception error) when (error is IOException or OperationCanceledException or ObjectDisposedException or System.Text.Json.JsonException)
            { _log("discord: clear not sent (" + error.GetType().Name + ")"); }
        }
        await DropAsync().ConfigureAwait(false);
    }

    public async ValueTask DisposeAsync()
    {
        await StopAsync(TimeSpan.FromSeconds(2)).ConfigureAwait(false);
        _stop.Dispose();
        _signal.Dispose();
    }
}
