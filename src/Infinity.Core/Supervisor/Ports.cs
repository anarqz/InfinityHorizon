// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Threading.Channels;
using Infinity.Core.Api;
using Infinity.Core.Auth;
using Infinity.Core.Process;
using Infinity.Core.Windows;

namespace Infinity.Core.Supervisor;

/// <summary>The gate as the supervisor uses it (GATE_API.md); <see cref="GateClient"/> over the API, fakes in tests.</summary>
public interface IGate
{
    Task<GateSession> CreateAsync(PlaySessionCreate create, CancellationToken cancellation);

    Task<GateSession> PollAsync(string sessionId, CancellationToken cancellation);

    Task<GateHeartbeatResponse> HeartbeatAsync(string sessionId, ulong? fenceToken, CancellationToken cancellation);

    Task LeaveAsync(string sessionId, string reason, ulong? fenceToken, CancellationToken cancellation);

    /// <summary>The leave with the line's revision after the final upload (<c>endRevision</c>, GATE_API.md).</summary>
    Task LeaveAsync(string sessionId, string reason, ulong? fenceToken, ulong? endRevision, CancellationToken cancellation) =>
        LeaveAsync(sessionId, reason, fenceToken, cancellation);
}

/// <summary><see cref="IGate"/> over <see cref="ApiClient"/>, with the access token from <see cref="TokenManager"/>.</summary>
public sealed class GateClient(ApiClient api, TokenManager tokens) : IGate
{
    public Task<GateSession> CreateAsync(PlaySessionCreate create, CancellationToken cancellation) =>
        tokens.WithAccessAsync(access => api.CreateSessionAsync(access, create, cancellation), cancellation);

    public Task<GateSession> PollAsync(string sessionId, CancellationToken cancellation) =>
        tokens.WithAccessAsync(access => api.SessionAsync(access, sessionId, cancellation), cancellation);

    public Task<GateHeartbeatResponse> HeartbeatAsync(string sessionId, ulong? fenceToken, CancellationToken cancellation) =>
        tokens.WithAccessAsync(access => api.HeartbeatAsync(access, sessionId, new GateHeartbeat(fenceToken), cancellation), cancellation);

    public Task LeaveAsync(string sessionId, string reason, ulong? fenceToken, CancellationToken cancellation) =>
        LeaveAsync(sessionId, reason, fenceToken, null, cancellation);

    public Task LeaveAsync(string sessionId, string reason, ulong? fenceToken, ulong? endRevision, CancellationToken cancellation) =>
        tokens.WithAccessAsync(async access =>
        {
            await api.LeaveAsync(access, sessionId, new GateLeave(reason, endRevision, fenceToken), cancellation).ConfigureAwait(false);
            return true;
        }, cancellation);
}

/// <summary>The running game as the supervisor sees it (<see cref="GameProcess"/> on Windows).</summary>
public interface IGameChild : IDisposable
{
    uint Pid { get; }

    ulong? Started { get; }

    /// <summary>Actual OS process exit time when available, before asynchronous world cleanup.</summary>
    DateTimeOffset? ExitTimeUtc => null;

    /// <summary>Signalled when the game process exits.</summary>
    WaitHandle Exited { get; }

    ChannelReader<JobEvent> JobEvents { get; }

    Task<uint> WaitForExitAsync(CancellationToken cancellation = default);

    /// <summary>The player's confirmed Quit: WM_CLOSE, then TerminateProcess after <paramref name="grace"/>.</summary>
    Task<bool> QuitAsync(TimeSpan grace, CancellationToken cancellation = default);
}

public interface IGameSpawner
{
    IGameChild Spawn(LaunchPlan plan);
}

/// <summary>The control pipe as the supervisor uses it (<see cref="PipeServer"/> on Windows).</summary>
public interface IControlPipe : IDisposable
{
    Task<AcceptResult> AcceptAsync(uint childPid, WaitHandle childExit, TimeSpan timeout, CancellationToken cancellation);

    Stream Stream { get; }

    Task WriteAsync(byte[] frame, CancellationToken cancellation);
}

[System.Runtime.Versioning.SupportedOSPlatform("windows")]
public sealed class WindowsSpawner : IGameSpawner
{
    public IGameChild Spawn(LaunchPlan plan) => GameProcess.Spawn(plan);
}

[System.Runtime.Versioning.SupportedOSPlatform("windows")]
public sealed class WindowsPipe(PipeServer server) : IControlPipe
{
    public Task<AcceptResult> AcceptAsync(uint childPid, WaitHandle childExit, TimeSpan timeout, CancellationToken cancellation) =>
        server.AcceptAsync(childPid, childExit, timeout, cancellation);

    public Stream Stream => server.Stream;

    public Task WriteAsync(byte[] frame, CancellationToken cancellation) => server.WriteAsync(frame, cancellation);

    public void Dispose() => server.Dispose();
}
