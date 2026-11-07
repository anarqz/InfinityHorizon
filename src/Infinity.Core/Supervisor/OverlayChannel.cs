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

    /// <summary>
    /// OD-192: while this says true (the player is in an official lobby, a private one included) no ordinary toast is sent or
    /// queued: every launcher notice passes here, so official play shows none and user lobbies show them all. T-L3 (fixes T-L6's
    /// M1): a toast marked <see cref="UiToastFlags.Always"/> (a private create's result and hold, the flagged Hardcore message;
    /// <see cref="Matchmaking.PrivateLobbyNotices"/>) passes in every lobby; invite prompts are their own frame and never pass here.
    /// </summary>
    public Func<bool>? Quiet { get; set; }

    /// <summary>The notice rule (T-L3): whether <paramref name="toast"/> shows while <paramref name="quiet"/> (an official lobby).</summary>
    public static bool Shows(UiFrame.Toast toast, bool quiet)
    {
        ArgumentNullException.ThrowIfNull(toast);
        return !quiet || (toast.Flags & UiToastFlags.Always) != 0;
    }

    /// <summary>A toast (queued until the overlay says hello); while <see cref="Quiet"/>, only one marked always (<see cref="Shows"/>).</summary>
    public Task ToastAsync(UiFrame.Toast toast, CancellationToken cancellation)
    {
        if (!Shows(toast, Quiet?.Invoke() == true)) return Task.CompletedTask;
        lock (_lock)
        {
            if (_closed) return Task.CompletedTask;
            _queued.Enqueue(OverlayNotices.Shortened(toast)); // OD-192: half as long
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

    /// <summary>Whether the connected overlay understands <paramref name="frame"/> (protocol 2 frames only to a protocol 2 overlay).</summary>
    public bool Accepts(UiFrame frame)
    {
        lock (_lock)
        {
            return !_closed && _session.Accepts(frame);
        }
    }

    /// <summary>The connected overlay's UI protocol (0 before its hello).</summary>
    public ushort Protocol
    {
        get
        {
            lock (_lock)
            {
                return _session.Protocol;
            }
        }
    }

    /// <summary>
    /// Any launcher → overlay frame (the worlds' answers); dropped while the overlay is not there, and
    /// dropped when the overlay's protocol does not have it (a protocol 1 overlay never sees 0x14-0x18).
    /// </summary>
    public Task SendFrameAsync(UiFrame frame, CancellationToken cancellation) =>
        Ready ? SendAsync(frame, cancellation) : Task.CompletedTask;

    /// <summary>
    /// A frame only a newer overlay has (OD-181's roster mods, sent after every "my world"): sent when this overlay's protocol
    /// has it, else skipped without the dropped-frame note (an older overlay simply shows no mods). A frame that cannot be
    /// encoded (server text past the frame's rules) is skipped and noted once, never thrown: the supervisor's seat and host
    /// loop that sends it must keep running.
    /// </summary>
    public async Task SendIfAcceptedAsync(UiFrame frame, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(frame);
        if (!Ready || !Accepts(frame)) return;
        try
        {
            await SendAsync(frame, cancellation).ConfigureAwait(false);
        }
        catch (ProtocolException)
        {
            if (Interlocked.Exchange(ref _unencodableNoted, 1) == 0)
                _incoming.Writer.TryWrite(new UiIncoming.Event(UiEventKind.Passthrough, $"ui pipe: skipped an unencodable type 0x{frame.Type:x2}"));
        }
    }

    private int _unencodableNoted;

    public Task PanelAsync(bool open, CancellationToken cancellation) =>
        Ready ? SendAsync(new UiFrame.Panel(open), cancellation) : Task.CompletedTask;

    private async Task SendAsync(UiFrame frame, CancellationToken cancellation)
    {
        // Every send path: a frame this overlay's protocol does not have is dropped (and noted), never written.
        if (!Accepts(frame))
        {
            bool open;
            lock (_lock) open = !_closed && _session.Ready;
            if (open) _incoming.Writer.TryWrite(new UiIncoming.Event(UiEventKind.Passthrough, $"ui pipe: dropped type 0x{frame.Type:x2} for protocol {Protocol}"));
            return;
        }
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
