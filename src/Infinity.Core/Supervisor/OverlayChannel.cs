// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Threading.Channels;
using Infinity.Core.Overlay;
using Infinity.Core.Windows;

namespace Infinity.Core.Supervisor;

/// <summary>The overlay DLL the hook loads (ORCHESTRATOR §4.2): its path and SHA-256, already checked by the launcher (OD-50).</summary>
public sealed record OverlayPackage(string Path, byte[] Sha256);

/// <summary>
/// The launcher's side of the UI pipe (<c>&lt;control pipe&gt;-ui</c>) for one game process: the
/// bounded accept repeated while the child lives (the overlay connects once the hook loaded it),
/// <see cref="UiSession"/>'s rules (the first frame is <c>UiHello</c> from the child), the current
/// status sent on hello, and the toasts queued in arrival order until then. What the overlay reports
/// (events, the panel, its goodbye) comes out of <see cref="Incoming"/>.
/// </summary>
public sealed class OverlayChannel : IDisposable
{
    private readonly IControlPipe _pipe;
    private readonly Channel<UiIncoming> _incoming = Channel.CreateUnbounded<UiIncoming>();
    private readonly SemaphoreSlim _write = new(1, 1);
    private readonly Queue<UiFrame.Toast> _queued = new();
    private readonly object _lock = new();
    private UiSession _session;
    private bool _toastReady, _flushing, _closed;

    public OverlayChannel(IControlPipe pipe, uint childPid, UiFrame.Status status)
    {
        _pipe = pipe ?? throw new ArgumentNullException(nameof(pipe));
        _session = new UiSession(childPid, status);
    }

    public ChannelReader<UiIncoming> Incoming => _incoming.Reader;

    public void Dispose() => _write.Dispose();

    public bool Ready
    {
        get
        {
            lock (_lock)
            {
                return !_closed && _session.Ready;
            }
        }
    }

    /// <summary>Accepts and reads until the child exits or <paramref name="cancellation"/>; a protocol fault closes the channel for this process.</summary>
    public async Task RunAsync(uint childPid, WaitHandle childExit, TimeSpan acceptBound, CancellationToken cancellation)
    {
        try
        {
            while (!cancellation.IsCancellationRequested)
            {
                var result = await _pipe.AcceptAsync(childPid, childExit, acceptBound, cancellation).ConfigureAwait(false);
                if (result == AcceptResult.ChildExited)
                {
                    return;
                }
                if (result == AcceptResult.Connected)
                {
                    await ReadAsync(cancellation).ConfigureAwait(false);
                    return; // one connection per game process
                }
            }
        }
        catch (Exception e) when (e is IOException or ProtocolException or ObjectDisposedException)
        {
            _incoming.Writer.TryWrite(new UiIncoming.Event(UiEventKind.Passthrough, "ui pipe: " + e.Message));
        }
        catch (OperationCanceledException) when (cancellation.IsCancellationRequested)
        {
        }
        finally
        {
            lock (_lock) { _closed = true; _toastReady = false; _queued.Clear(); }
            _incoming.Writer.TryComplete();
        }
    }

    /// <summary>The status line (sent now when the overlay is ready, else on its hello).</summary>
    public async Task SetStatusAsync(UiFrame.Status status, CancellationToken cancellation)
    {
        UiFrame.Status? now;
        lock (_lock)
        {
            now = _session.SetStatus(status);
        }
        if (now is not null)
        {
            await SendAsync(now, cancellation).ConfigureAwait(false);
        }
    }

    /// <summary>A toast (queued until the overlay says hello).</summary>
    public Task ToastAsync(UiFrame.Toast toast, CancellationToken cancellation)
    {
        lock (_lock)
        {
            if (_closed) return Task.CompletedTask;
            _queued.Enqueue(toast);
            if (!_toastReady || _flushing) return Task.CompletedTask;
            _flushing = true;
        }
        return FlushToastsAsync(cancellation);
    }

    private async Task FlushToastsAsync(CancellationToken cancellation)
    {
        try
        {
            while (true)
            {
                UiFrame.Toast toast;
                lock (_lock)
                {
                    if (_closed || !_queued.TryDequeue(out toast!)) { _flushing = false; return; }
                }
                await SendAsync(toast, cancellation).ConfigureAwait(false);
            }
        }
        catch
        {
            lock (_lock) { _flushing = false; }
            throw;
        }
    }

    /// <summary>Any launcher → overlay frame (the worlds' answers); dropped while the overlay is not there.</summary>
    public Task SendFrameAsync(UiFrame frame, CancellationToken cancellation) =>
        Ready ? SendAsync(frame, cancellation) : Task.CompletedTask;

    public Task PanelAsync(bool open, CancellationToken cancellation) =>
        Ready ? SendAsync(new UiFrame.Panel(open), cancellation) : Task.CompletedTask;

    private async Task SendAsync(UiFrame frame, CancellationToken cancellation)
    {
        await _write.WaitAsync(cancellation).ConfigureAwait(false);
        try
        {
            await _pipe.WriteAsync(frame.Encode(), cancellation).ConfigureAwait(false);
        }
        catch (IOException)
        {
            // the overlay went away (passthrough, the game closing): nothing to show it on
        }
        finally
        {
            _write.Release();
        }
    }

    private async Task ReadAsync(CancellationToken cancellation)
    {
        var buffer = new byte[64 * 1024 + 8];
        var filled = 0;
        while (!cancellation.IsCancellationRequested)
        {
            var read = await _pipe.Stream.ReadAsync(buffer.AsMemory(filled), cancellation).ConfigureAwait(false);
            if (read == 0)
            {
                return;
            }
            filled += read;
            while (true)
            {
                var frame = UiFrame.Decode(buffer.AsSpan(0, filled), out var used);
                if (frame is null)
                {
                    break;
                }
                Buffer.BlockCopy(buffer, used, buffer, 0, filled - used);
                filled -= used;
                UiIncoming incoming;
                List<UiFrame> replies = [];
                lock (_lock)
                {
                    incoming = _session.OnFrame(frame);
                    if (incoming is UiIncoming.Hello hello)
                    {
                        replies.AddRange(hello.Reply);
                    }
                }
                foreach (var reply in replies)
                {
                    await SendAsync(reply, cancellation).ConfigureAwait(false);
                }
                if (incoming is UiIncoming.Hello)
                {
                    bool flush;
                    lock (_lock)
                    {
                        _toastReady = true;
                        flush = !_flushing;
                        if (flush) _flushing = true;
                    }
                    if (flush) await FlushToastsAsync(cancellation).ConfigureAwait(false);
                }
                _incoming.Writer.TryWrite(incoming);
            }
            if (filled == buffer.Length)
            {
                throw new ProtocolException("ui frame larger than the buffer");
            }
        }
    }
}
