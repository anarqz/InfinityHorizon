// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO.Pipes;
using System.Text;
using Infinity.Core.Presence;

namespace Infinity.Core.Process;

/// <summary>
/// CR-L41.2: a second launch with an <c>infinity-horizon://join/…</c> link hands the link to the launcher that holds
/// <c>launcher.lock</c> (<see cref="LauncherLock"/>) and exits. The holder listens on a pipe named after its pid
/// (<c>InfinityHorizon.Launcher.&lt;pid&gt;</c>, this user only); the second process reads the pid from the lock, sends one
/// line (the world id) and is done. Only a valid world id is ever passed on, and receiving one only fills the
/// launcher's Join from link field: nothing joins without the player.
/// </summary>
public sealed class LauncherHandoff : IDisposable
{
    public static readonly TimeSpan SendTimeout = TimeSpan.FromSeconds(2);
    private const int MaxLine = 128;

    private readonly CancellationTokenSource _stop = new();
    private readonly string _name;
    private readonly Action<string> _received;
    private readonly Action<string> _log;

    private LauncherHandoff(string name, Action<string> received, Action<string> log)
    {
        _name = name;
        _received = received;
        _log = log;
    }

    public static string PipeName(uint pid) => "InfinityHorizon.Launcher." + pid.ToString(CultureInfo.InvariantCulture);

    /// <summary>Listens for links as the lock holder <paramref name="pid"/>; <paramref name="received"/> gets each world id (off the UI thread).</summary>
    public static LauncherHandoff Listen(uint pid, Action<string> received, Action<string>? log = null)
    {
        ArgumentNullException.ThrowIfNull(received);
        var handoff = new LauncherHandoff(PipeName(pid), received, log ?? (_ => { }));
        _ = Task.Run(handoff.ServeAsync);
        return handoff;
    }

    private async Task ServeAsync()
    {
        var token = _stop.Token;
        while (!token.IsCancellationRequested)
        {
            try
            {
                await using var server = new NamedPipeServerStream(_name, PipeDirection.In, 1, PipeTransmissionMode.Byte,
                    PipeOptions.Asynchronous | PipeOptions.CurrentUserOnly);
                await server.WaitForConnectionAsync(token).ConfigureAwait(false);
                using var bound = CancellationTokenSource.CreateLinkedTokenSource(token);
                bound.CancelAfter(SendTimeout);
                var buffer = new byte[MaxLine + 1];
                var length = 0;
                while (length < buffer.Length)
                {
                    var read = await server.ReadAsync(buffer.AsMemory(length), bound.Token).ConfigureAwait(false);
                    if (read == 0) break;
                    length += read;
                }
                if (length > MaxLine) { _log("link: hand-off refused (too long)"); continue; }
                if (JoinLink.WorldId(Encoding.UTF8.GetString(buffer, 0, length)) is { } worldId) _received(worldId);
                else _log("link: hand-off refused (not a world id)");
            }
            catch (OperationCanceledException) when (token.IsCancellationRequested) { return; }
            catch (Exception error) when (error is IOException or OperationCanceledException or UnauthorizedAccessException)
            {
                _log("link: hand-off listener " + error.GetType().Name);
                try { await Task.Delay(TimeSpan.FromSeconds(1), token).ConfigureAwait(false); }
                catch (OperationCanceledException) { return; }
            }
        }
    }

    /// <summary>
    /// Sends <paramref name="worldId"/> to the launcher holding the lock of <paramref name="stateDirectory"/>; false when there is
    /// none (or it does not answer): this process then starts normally and takes the link itself.
    /// </summary>
    public static bool TrySend(string stateDirectory, string worldId)
    {
        if (!JoinLink.IsWorldId(worldId) || LauncherLock.ReadHolder(stateDirectory) is not { } holder || holder.Pid == (uint)Environment.ProcessId) return false;
        return TrySendTo(PipeName(holder.Pid), worldId);
    }

    internal static bool TrySendTo(string pipeName, string worldId)
    {
        try
        {
            using var client = new NamedPipeClientStream(".", pipeName, PipeDirection.Out, PipeOptions.CurrentUserOnly);
            client.Connect((int)SendTimeout.TotalMilliseconds);
            client.Write(Encoding.UTF8.GetBytes(worldId));
            client.Flush();
            return true;
        }
        catch (Exception error) when (error is TimeoutException or IOException or UnauthorizedAccessException)
        {
            return false;
        }
    }

    internal static LauncherHandoff ListenOn(string pipeName, Action<string> received, Action<string>? log = null)
    {
        var handoff = new LauncherHandoff(pipeName, received, log ?? (_ => { }));
        _ = Task.Run(handoff.ServeAsync);
        return handoff;
    }

    public void Dispose()
    {
        _stop.Cancel();
        _stop.Dispose();
    }
}
