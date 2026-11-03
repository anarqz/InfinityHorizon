// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.WorldServer;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// A world server the bridge hosts a world on, with its admission: the in-process engine (<see cref="IntegratedWorldRelay"/>)
/// or the <c>dedicated-server</c> child (<c>DedicatedServer.DedicatedWorldRelay</c>, CR-L50). Everything the bridge, the
/// overlay and the away-host read comes through here and <see cref="IHostRelay"/>, whichever engine runs the world.
/// </summary>
public interface IWorldHostRelay : IHostRelay, IAsyncDisposable
{
    /// <summary>The launcher's log for the world server's lines (ids and counters only).</summary>
    Action<string>? DiagnosticLog { get; set; }

    /// <summary>The session's bounded crash context: validation refusals land here, never in a log.</summary>
    Diagnostics.CrashContext? CrashContext { get; set; }

    /// <summary>The platform call a car meet invitation makes for its shared session (kind, XUIDs); null: refused.</summary>
    Func<string, IReadOnlyList<string>, CancellationToken, Task<(string Name, string Template)?>>? GameSessions { get; set; }

    /// <summary>The world socket's discovery state (NAT, public addresses, UPnP), for the announce and the details screen.</summary>
    RelayNetworkStatus NetworkStatus { get; }

    /// <summary>
    /// True once the world server ended on its own (a process exit, its channel's end, a world it closed unasked): the world
    /// is gone and is never re-opened automatically (DEDICATED_SERVER.md §2.2). The in-process engine never says so.
    /// </summary>
    bool EngineEnded => false;

    /// <summary>A PUT or announce/heartbeat answer: the world context (renews the 45 s host lease), then its punches.</summary>
    Task UpdateWorldAsync(WorldAnnounced world, CancellationToken cancellation = default);

    /// <summary>A <c>GET /mm/v1/punches</c> read: the admission snapshot only, never the lease.</summary>
    Task RefreshAdmissionsAsync(IMatchmakingApi matchmaking, CancellationToken cancellation = default);

    /// <summary>OD-154, CR-L46: the host's voice mode (<see cref="VoiceModes"/>) and lobby mutes.</summary>
    void SetVoice(string mode, IReadOnlyCollection<ulong> mutedForLobby);

    /// <summary>CR-S67: every member's <c>voiceBlockedUntil</c> from the host's roster poll.</summary>
    void SetRosterVoiceBlocks(IEnumerable<WorldMember> members);
}

/// <summary>
/// AC-2 for a world server that takes the wire rules (MATCHMAKING_API §3.9) as they are and loads its own parts catalog: the
/// bridge hands it the rules, not <see cref="PeerWatchRules"/>. <paramref name="report"/>'s second argument is the episode's id
/// as the engine minted it (posted as <c>PeerObservation.id</c>); null: the launcher mints one.
/// </summary>
public interface IWireWatchRelay
{
    void WatchWire(WatchRules? rules, bool forceSpeed, bool parts, ulong self, Action<PeerWatchReport, string?>? report);
}

/// <summary>What a world server is started for: the world's players and its id, minted once per world (DEDICATED_SERVER.md §4.3).</summary>
public sealed record HostRelayStart(uint MaxPlayers, string WorldId);
