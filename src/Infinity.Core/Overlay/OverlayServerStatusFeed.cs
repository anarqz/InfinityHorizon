// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Overlay;

/// <summary>
/// The guide's bottom block (CR-L40, LOBBIES_AND_SERVER_STATUS.md §1.2-§1.3): the connected server's
/// <see cref="UiFrame.ServerStatus"/> on the overlay's hello and then at most every 30 s while the guide is
/// open (the overlay's <c>Panel</c> reports), and the player's <see cref="UiFrame.Avatar"/> on hello and on
/// every change. The read is the server's public <c>/core/v1/status</c>; a failed read sends "unknown",
/// never zeros. One feed per play session; <see cref="AttachAsync"/> again for a new overlay connection.
/// </summary>
public sealed class OverlayServerStatusFeed
{
    public static readonly TimeSpan Every = TimeSpan.FromSeconds(30);
    public static readonly TimeSpan ReadBound = TimeSpan.FromSeconds(10);

    private readonly string _server;
    private readonly Func<CancellationToken, Task<Api.ServerStatus?>> _read;
    private readonly TimeProvider _clock;
    private readonly Action<string> _log;
    private readonly object _lock = new();
    private TaskCompletionSource _wake = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private Func<UiFrame, CancellationToken, Task>? _send;
    private CancellationToken _attached;
    private UiFrame.Avatar _avatar = UiFrame.Avatar.None;
    private bool _open;
    private long? _lastRead;
    private long _generation;

    /// <param name="server">The host shown under the status (the core origin's authority).</param>
    /// <param name="read">The status read (<see cref="Api.ApiClient.ServerStatusAsync(CancellationToken)"/>); null or a throw: unknown.</param>
    public OverlayServerStatusFeed(string server, Func<CancellationToken, Task<Api.ServerStatus?>> read, TimeProvider? clock = null, Action<string>? log = null)
    {
        _server = server ?? string.Empty;
        _read = read ?? throw new ArgumentNullException(nameof(read));
        _clock = clock ?? TimeProvider.System;
        _log = log ?? (_ => { });
    }

    /// <summary>The frame last sent or about to be (for tests and the log).</summary>
    public UiFrame.ServerStatus? Last { get; private set; }

    /// <summary>The avatar to show (<see cref="UiFrame.Avatar.None"/>: the initials); sent now when an overlay is attached.</summary>
    public UiFrame.Avatar Avatar
    {
        get { lock (_lock) return _avatar; }
        set
        {
            ArgumentNullException.ThrowIfNull(value);
            Func<UiFrame, CancellationToken, Task>? send;
            CancellationToken token;
            lock (_lock) { _avatar = value; (send, token) = (_send, _attached); }
            if (send is not null) _ = SendAsync(send, value, token);
        }
    }

    /// <summary>
    /// The overlay said hello: the avatar and a fresh status now, then the 30 s refresh while the guide is open
    /// until <paramref name="cancellation"/> (the session's end). Returns once the first two frames went out.
    /// </summary>
    public async Task AttachAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(send);
        UiFrame.Avatar avatar;
        long generation;
        lock (_lock) { _send = send; _attached = cancellation; avatar = _avatar; generation = ++_generation; _open = false; }
        await SendAsync(send, avatar, cancellation).ConfigureAwait(false);
        await RefreshAsync(send, cancellation).ConfigureAwait(false);
        _ = LoopAsync(send, generation, cancellation);
    }

    /// <summary>The overlay's <c>Panel{open}</c>: the guide opened or closed.</summary>
    public void Panel(bool open)
    {
        TaskCompletionSource wake;
        lock (_lock)
        {
            _open = open;
            wake = _wake;
            _wake = new(TaskCreationOptions.RunContinuationsAsynchronously);
        }
        wake.TrySetResult();
    }

    private async Task LoopAsync(Func<UiFrame, CancellationToken, Task> send, long generation, CancellationToken cancellation)
    {
        try
        {
            while (!cancellation.IsCancellationRequested)
            {
                Task wake;
                bool open;
                TimeSpan due;
                lock (_lock)
                {
                    if (generation != _generation) return; // a newer connection owns the refresh
                    (wake, open) = (_wake.Task, _open);
                    due = _lastRead is { } last ? Every - _clock.GetElapsedTime(last) : TimeSpan.Zero;
                }
                if (!open) { await wake.WaitAsync(cancellation).ConfigureAwait(false); continue; }
                if (due > TimeSpan.Zero)
                {
                    await Task.WhenAny(wake, Task.Delay(due, _clock, cancellation)).ConfigureAwait(false);
                    continue;
                }
                await RefreshAsync(send, cancellation).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested) { }
    }

    private async Task RefreshAsync(Func<UiFrame, CancellationToken, Task> send, CancellationToken cancellation)
    {
        lock (_lock) _lastRead = _clock.GetTimestamp();
        Api.ServerStatus? status = null;
        try
        {
            using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
            bound.CancelAfter(ReadBound);
            status = await _read(bound.Token).WaitAsync(bound.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested) { throw; }
        catch (Exception error) when (error is HttpRequestException or IOException or InvalidDataException or OperationCanceledException
            or TimeoutException or System.Text.Json.JsonException or Api.ApiException or ArgumentException)
        {
            _log("overlay: server status unavailable (" + error.GetType().Name + ")");
        }
        var frame = UiFrame.ServerStatus.From(_server, status);
        Last = frame;
        await SendAsync(send, frame, cancellation).ConfigureAwait(false);
    }

    private async Task SendAsync(Func<UiFrame, CancellationToken, Task> send, UiFrame frame, CancellationToken cancellation)
    {
        try { await send(frame, cancellation).ConfigureAwait(false); }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested) { }
        catch (Exception error) when (error is IOException or ObjectDisposedException or ProtocolException or InvalidOperationException)
        {
            _log($"overlay: type 0x{frame.Type:x2} not sent ({error.GetType().Name})");
        }
    }
}
