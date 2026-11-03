// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;
using System.Text.Json.Serialization;
using Infinity.Core.Matchmaking;

namespace Infinity.Core.DedicatedServer;

// The control channel of dedicated-server (docs/contracts/DEDICATED_SERVER.md §3-§4, proto 1), mirrored
// from native/dedicated-server field for field. Field names are camelCase; XUIDs are decimal strings;
// times are Unix milliseconds (…Ms) or Unix seconds (voice blocks); ids are lowercase UUID strings.
// Optional fields are nullable and left out when null, except where null itself means something
// (world.watch rules, world.gameSession.answer session). Unknown reply and event fields are ignored.

/// <summary>The protocol range this controller speaks (§10).</summary>
public static class DsProto
{
    /// <summary>The <c>hello.proto</c> this controller asks for (an integer, §10).</summary>
    public const int Version = 1;

    /// <summary><c>busy</c> without <c>retryAfterMs</c>: the engine's documented default (§4.1).</summary>
    public const long DefaultBusyRetryMs = 1000;

    /// <summary>The longest line either side writes (§3.2).</summary>
    public const int MaxLine = 1024 * 1024;

    /// <summary>OD-186: the game a world serves; the only one so far.</summary>
    public const string GameFh2 = "fh2";

    /// <summary>The environment variable of the per-launch secret (§2, environment only).</summary>
    public const string SecretEnv = "FI_DS_CONTROL_SECRET";

    /// <summary>The prefix of every variable the engine reads; none is inherited from the launcher's environment.</summary>
    public const string EnvPrefix = "FI_DS_";
}

/// <summary>Request names (§4.2-§4.4).</summary>
public static class DsOps
{
    public const string Hello = "hello";
    public const string KeysSet = "keys.set";
    public const string CatalogSet = "catalog.set";
    public const string CatalogParts = "catalog.parts";
    public const string Capacity = "capacity";
    public const string Health = "health";
    public const string Stats = "stats";
    public const string Drain = "drain";
    public const string Shutdown = "shutdown";
    public const string WorldOpen = "world.open";
    public const string WorldContext = "world.context";
    public const string WorldAdmission = "world.admission";
    public const string WorldAdmissionEnd = "world.admission.end";
    public const string WorldVoice = "world.voice";
    public const string WorldVoiceBlocks = "world.voiceBlocks";
    public const string WorldWatch = "world.watch";
    public const string WorldKick = "world.kick";
    public const string WorldClose = "world.close";
    public const string WorldStatus = "world.status";
    public const string GameSessionAnswer = "world.gameSession.answer";
}

/// <summary>Error codes (§4.1).</summary>
public static class DsErrorCodes
{
    public const string BadRequest = "bad_request";
    public const string UnknownOp = "unknown_op";
    public const string NotReady = "not_ready";
    public const string Unauthorized = "unauthorized";
    public const string ProtoUnsupported = "proto_unsupported";
    public const string Busy = "busy";
    public const string Draining = "draining";
    public const string Capacity = "capacity";
    public const string Exists = "exists";
    public const string NotFound = "not_found";
    public const string BindFailed = "bind_failed";
    public const string NoKeys = "no_keys";
    public const string ContextInvalid = "context_invalid";
    public const string Stale = "stale";
    public const string Internal = "internal";

    /// <summary>Client-side, never on the wire: a reply that did not parse as its result (a typed protocol error).</summary>
    public const string MalformedReply = "malformed_reply";
}

/// <summary>Event names (§4.5).</summary>
public static class DsEventNames
{
    public const string WorldState = "world.state";
    public const string PeerSynced = "peer.synced";
    public const string PeerLeft = "peer.left";
    public const string WorldEmpty = "world.empty";
    public const string WorldOccupied = "world.occupied";
    public const string WorldClosed = "world.closed";
    public const string WorldNetwork = "world.network";
    public const string GameSessionRequest = "world.gameSession.request";
    public const string WatchReport = "watch.report";
    public const string PeerRejected = "peer.rejected";
    public const string Diag = "diag";
    public const string Notice = "notice";
    public const string Draining = "ds.draining";
    public const string EventsDropped = "events.dropped";
    public const string CatalogNeed = "catalog.need";
}

/// <summary>Stable notice ids (§4.7) the launcher maps to toasts or log lines.</summary>
public static class DsNoticeIds
{
    public const string BindFailed = "ds.world.bind_failed";
    public const string NoRoadTrip = "ds.world.no_roadtrip";
    public const string NoParts = "ds.world.no_parts";
    public const string AdmissionLapsed = "ds.admission.lapsed";
    public const string AdmissionCleared = "ds.admission.cleared";
    public const string UpnpMapped = "ds.upnp.mapped";
    public const string UpnpFailed = "ds.upnp.failed";
    public const string CapacityRefused = "ds.capacity.refused";
    public const string CpuGuard = "ds.cpu.guard";
    public const string MemoryHigh = "ds.memory.high";
    public const string SessionCancelled = "ds.carmeet.session_cancelled";
    public const string ControlStalled = "ds.control.stalled";
}

/// <summary>An error reply's <c>error</c> (§3.2).</summary>
public sealed record DsError
{
    public required string Code { get; init; }

    public string? Field { get; init; }

    public string? Reason { get; init; }

    public long? RetryAfterMs { get; init; }

    public override string ToString() => Field is null ? Code : $"{Code} ({Field})";
}

/// <summary>A request: its <c>op</c> and, for world commands, the world it names (the busy retry's key).</summary>
public interface IDsRequest
{
    /// <summary>The request's <c>op</c> (written first, after <c>id</c>).</summary>
    string Op { get; }

    /// <summary>The world a world command names; null for process commands.</summary>
    string? TargetWorld() => null;
}

// ---- process commands (§4.2) ----

public sealed record DsControllerInfo
{
    /// <summary><c>launcher</c> or <c>vm</c>.</summary>
    public required string Kind { get; init; }

    public required string Version { get; init; }
}

public sealed record DsHello : IDsRequest
{
    public string Op => DsOps.Hello;

    /// <summary>The protocol version asked for (an integer, §10); the engine answers it or <c>proto_unsupported</c>.</summary>
    public required int Proto { get; init; }

    /// <summary>64 lowercase hex; pipe only. Never logged (<see cref="ToString"/> redacts it).</summary>
    public string? Secret { get; init; }

    public DsControllerInfo? Controller { get; init; }

    public override string ToString() => $"DsHello {{ proto = {Proto}, secret = {(Secret is null ? "none" : "redacted")} }}";
}

public sealed record DsBuild
{
    public required string Version { get; init; }

    public required string Commit { get; init; }
}

public sealed record DsMemory
{
    public long UsedMb { get; init; }

    public long LimitMb { get; init; }

    public long? ReservedMb { get; init; }
}

public sealed record DsCapacity
{
    public int MaxWorlds { get; init; }

    public int MaxSeats { get; init; }

    public int OpenWorlds { get; init; }

    public int Seats { get; init; }

    public int FreeSeats { get; init; }

    public DsMemory? Memory { get; init; }

    public bool CpuGuard { get; init; }
}

public sealed record DsHelloResult
{
    /// <summary>The highest common version.</summary>
    public required int Proto { get; init; }

    public required DsBuild Build { get; init; }

    public required string Profile { get; init; }

    public DsCapacity? Capacity { get; init; }

    public bool Draining { get; init; }

    public IReadOnlyList<string> Worlds { get; set; } = [];

    public bool Keys { get; init; }
}

public sealed record DsKeysSet : IDsRequest
{
    public string Op => DsOps.KeysSet;

    /// <summary>The <c>RelayGrantVerifyKeys</c> text form, <c>kid:base64,…</c> (≤ 8 keys, ≤ 1024 chars).</summary>
    public required string Ring { get; init; }
}

public sealed record DsKeysSetResult
{
    public int Count { get; init; }
}

/// <summary>A request with no fields (<c>capacity</c>, <c>health</c>, <c>stats</c>, <c>shutdown</c>).</summary>
public sealed record DsBare : IDsRequest
{
    public DsBare(string op) => Op = op;

    public string Op { get; }
}

public sealed record DsLag
{
    public long Last { get; init; }

    public long P99 { get; init; }
}

public sealed record DsHealth
{
    public bool Ready { get; init; }

    public DsLag? LoopLagMs { get; init; }

    public long UptimeMs { get; init; }

    public int Worlds { get; init; }

    public int Seats { get; init; }

    public DsMemory? Memory { get; init; }
}

public sealed record DsDrain : IDsRequest
{
    public string Op => DsOps.Drain;

    /// <summary>0-300; absent: the process's <c>--drain-seconds</c>.</summary>
    public int? DeadlineSec { get; init; }
}

/// <summary>An empty result (<c>{}</c>).</summary>
public sealed record DsEmpty;

// ---- world commands (§4.3) ----

/// <summary><c>world.open.host</c> of the private profile: <c>{kind: "player", xuid, isPlayer}</c> (§4.3, §6.3; <c>official</c> is the VM's).</summary>
public sealed record DsHost
{
    public const string PlayerKind = "player";

    /// <summary>Always <c>player</c> from the launcher (the private profile refuses any other kind).</summary>
    public string Kind { get; set; } = PlayerKind;

    public required string Xuid { get; init; }

    /// <summary>False for an OD-109 dedicated host (§6.3).</summary>
    public required bool IsPlayer { get; init; }
}

public sealed record DsVoice
{
    /// <summary><c>off</c>, <c>open</c> or <c>proximity</c> (strict, §14 item 4).</summary>
    public required string Mode { get; init; }

    public IReadOnlyList<string> LobbyMuted { get; set; } = [];
}

/// <summary><c>WatchSpec</c> (§4.3): <c>world.open.watch</c> as is; <see cref="DsWorldWatch"/> is <c>{worldId} &amp; WatchSpec</c>.</summary>
public sealed record DsWatchSpec
{
    /// <summary>MATCHMAKING_API §3.9 verbatim; null: stop.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.Never)]
    public WatchRules? Rules { get; init; }

    /// <summary>The host's own XUID, never measured; absent when <c>host.isPlayer</c> is false.</summary>
    public string? Self { get; init; }

    public required bool Parts { get; init; }

    /// <summary>Tests only.</summary>
    public bool? ForceSpeed { get; init; }
}

public sealed record DsNetworkOptions
{
    public int Port { get; set; } = HostRelay.DefaultPort;

    public bool Discovery { get; set; } = true;

    public bool Upnp { get; set; } = true;

    public IReadOnlyList<string>? StunServers { get; init; }

    public long? StunIntervalMs { get; init; }
}

public sealed record DsWorldOpen : IDsRequest
{
    public string Op => DsOps.WorldOpen;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }

    /// <summary>OD-186: the game module that serves the world.</summary>
    public string Game { get; set; } = DsProto.GameFh2;

    public required DsHost Host { get; init; }

    /// <summary>1-12 (the launcher sends the PUT's 2-12).</summary>
    public required int MaxPlayers { get; init; }

    public bool Races { get; set; } = true;

    public required bool RequireProof { get; init; }

    public bool? DevLanPeers { get; init; }

    public DsVoice? Voice { get; init; }

    public DsWatchSpec? Watch { get; init; }

    public DsNetworkOptions? Network { get; init; }

    /// <summary>Engine overrides for tests and the VM profile only (§8).</summary>
    public IReadOnlyDictionary<string, long>? Settings { get; init; }
}

public sealed record DsPublicAddress
{
    public required string Address { get; init; }

    public required int Port { get; init; }
}

/// <summary>The raw <c>RelayNetworkStatus</c> plus the bound port (§4.5, private only).</summary>
public sealed record DsNetworkFacts
{
    public string? WorldId { get; init; }

    public int Port { get; init; }

    /// <summary><c>unknown</c>, <c>cone</c> or <c>symmetric</c>.</summary>
    public string Nat { get; set; } = "unknown";

    public IReadOnlyList<DsPublicAddress> PublicAddresses { get; set; } = [];

    /// <summary><c>off</c>, <c>pending</c>, <c>not_found</c>, <c>mapped</c> or <c>failed</c>.</summary>
    public string Upnp { get; set; } = "off";

    public DsPublicAddress? UpnpAddress { get; init; }

    public bool BehindAnotherNat { get; init; }

    public bool DualStack { get; init; }

    public long PunchHints { get; init; }

    public long PunchesSent { get; init; }

    public long SendErrors { get; init; }

    public override string ToString() => $"DsNetworkFacts {{ port = {Port}, nat = {Nat}, upnp = {Upnp}, addresses = redacted }}";
}

public sealed record DsWorldOpenResult
{
    public required string WorldId { get; init; }

    /// <summary><c>["carmeet"]</c> or <c>["carmeet","roadtrip"]</c>: the PUT's <c>WorldSettings.lobbies</c>.</summary>
    public IReadOnlyList<string> Lobbies { get; set; } = [];

    public DsNetworkFacts? Network { get; init; }
}

public sealed record DsContextHost
{
    public required string Xuid { get; init; }
}

public sealed record DsContextWorld
{
    public required string Id { get; init; }

    public string Game { get; set; } = DsProto.GameFh2;

    public required DsContextHost Host { get; init; }
}

/// <summary>After the PUT and every announce/heartbeat answer: the world context and the 45 s host lease only, never punches (§4.3; the punches follow in <see cref="DsWorldAdmission"/>).</summary>
public sealed record DsWorldContext : IDsRequest
{
    public string Op => DsOps.WorldContext;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }

    /// <summary>The trusted platform time when proof is required (§6.2 "Time").</summary>
    public required long ServerTimeMs { get; init; }

    [JsonPropertyName("world")]
    public required DsContextWorld WorldInfo { get; init; }

    public override string ToString() => $"DsWorldContext {{ world = {WorldId} }}";
}

/// <summary>The punches only (the announce answer's and every <c>GET /mm/v1/punches</c> read): replaces the admission snapshot without renewing the lease.</summary>
public sealed record DsWorldAdmission : IDsRequest
{
    public string Op => DsOps.WorldAdmission;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }

    public required long ServerTimeMs { get; init; }

    public required IReadOnlyList<Punch> Punches { get; init; }

    public override string ToString() => $"DsWorldAdmission {{ world = {WorldId}, punches = {Punches.Count} }}";
}

/// <summary>The <c>world.context</c> reply: the host lease's new end.</summary>
public sealed record DsContextResult
{
    public long? LeaseUntilMs { get; init; }
}

public sealed record DsAdmissionResult
{
    public int Admitted { get; init; }

    public int Legs { get; init; }

    public int ProofGrants { get; init; }
}

public sealed record DsWorldAdmissionEnd : IDsRequest
{
    public string Op => DsOps.WorldAdmissionEnd;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }

    /// <summary><c>pause</c>, <c>unauthorized</c>, <c>not_found</c>, <c>not_host</c> or <c>wrong_pool</c>.</summary>
    public required string Reason { get; init; }
}

public sealed record DsWorldVoice : IDsRequest
{
    public string Op => DsOps.WorldVoice;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }

    public required string Mode { get; init; }

    public IReadOnlyList<string> LobbyMuted { get; set; } = [];
}

public sealed record DsWorldVoiceBlocks : IDsRequest
{
    public string Op => DsOps.WorldVoiceBlocks;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }

    public string Source { get; set; } = "roster";

    /// <summary>XUID → Unix seconds (0 = none), at most 64 (<see cref="DsCommands.VoiceBlocks"/>).</summary>
    public required IReadOnlyDictionary<string, long> Blocks { get; init; }
}

public sealed record DsVoiceBlocksResult
{
    public bool Changed { get; init; }
}

/// <summary><c>WatchCommand = {worldId} &amp; WatchSpec</c> (§4.3).</summary>
public sealed record DsWorldWatch : IDsRequest
{
    public static DsWorldWatch From(string worldId, DsWatchSpec spec)
    {
        ArgumentNullException.ThrowIfNull(spec);
        return new DsWorldWatch { WorldId = worldId, Rules = spec.Rules, Self = spec.Self, Parts = spec.Parts, ForceSpeed = spec.ForceSpeed };
    }

    public string Op => DsOps.WorldWatch;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.Never)]
    public WatchRules? Rules { get; init; }

    public string? Self { get; init; }

    public required bool Parts { get; init; }

    public bool? ForceSpeed { get; init; }
}

public sealed record DsWorldKick : IDsRequest
{
    public string Op => DsOps.WorldKick;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }

    public required string Xuid { get; init; }
}

public sealed record DsKickResult
{
    public int Dropped { get; init; }
}

public sealed record DsWorldClose : IDsRequest
{
    public string Op => DsOps.WorldClose;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }

    /// <summary>A text id; <c>shutdown</c>/<c>drained</c> send the game's Close to peers.</summary>
    public required string Reason { get; init; }
}

public sealed record DsWorldStatus : IDsRequest
{
    public string Op => DsOps.WorldStatus;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }
}

public sealed record DsGameSession
{
    public required string Name { get; init; }

    public required string TemplateName { get; init; }
}

/// <summary>The answer to <see cref="DsGameSessionRequest"/> (§4.4), within 14 s; null session cancels.</summary>
public sealed record DsGameSessionAnswer : IDsRequest
{
    public string Op => DsOps.GameSessionAnswer;

    public string? TargetWorld() => WorldId;

    public required string WorldId { get; init; }

    public required uint Rid { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.Never)]
    public DsGameSession? Session { get; init; }

    public string? Error { get; init; }
}

// ---- game-data catalogs (§9, CR-L51) ----

/// <summary>One Road Trip event as the reference reads it (<c>RoadTripCatalog.Load</c>): the event, its location, mode, players and classes.</summary>
public sealed record DsRoadTripEvent
{
    public required int Id { get; init; }

    public required int Location { get; init; }

    public required int Mode { get; init; }

    public required int MinPlayers { get; init; }

    /// <summary>CarClassId 0-7, at most 8.</summary>
    public required IReadOnlyList<int> Classes { get; init; }
}

public sealed record DsRoadTrip
{
    /// <summary>At most <see cref="DsCatalogs.MaxEvents"/>, in database order.</summary>
    public required IReadOnlyList<DsRoadTripEvent> Events { get; init; }
}

/// <summary><c>catalog.set</c> (§4.2, §9): process-wide, latest-wins, idempotent for the same version and rows.</summary>
public sealed record DsCatalogSet : IDsRequest
{
    public string Op => DsOps.CatalogSet;

    public string Game { get; init; } = DsProto.GameFh2;

    /// <summary>1-64 printable ASCII; the same rows give the same version.</summary>
    public required string Version { get; init; }

    public required DsRoadTrip RoadTrip { get; init; }

    public override string ToString() => $"DsCatalogSet {{ version = {Version}, events = {RoadTrip.Events.Count} }}";
}

public sealed record DsCatalogSetResult
{
    public bool Changed { get; init; }

    public int Events { get; init; }
}

/// <summary><c>catalog.parts</c> (§9): a model's rows per slot table; no entry at all is a model this database does not have.</summary>
public sealed record DsCatalogParts : IDsRequest
{
    public string Op => DsOps.CatalogParts;

    public string Game { get; init; } = DsProto.GameFh2;

    public required uint Model { get; init; }

    /// <summary>Table name (§9's 43) → row ids; ≤ 1024 per table, ≤ 4096 in all.</summary>
    public required IReadOnlyDictionary<string, IReadOnlyList<uint>> Slots { get; init; }

    public override string ToString() => $"DsCatalogParts {{ model = {Model}, tables = {Slots.Count} }}";
}

/// <summary><c>catalog.need</c> (§4.5): the parts rule needs a model's rows (process-level, no world).</summary>
public sealed record DsCatalogNeed : DsEvent
{
    public required string Game { get; init; }

    public required string Kind { get; init; }

    public required uint Model { get; init; }
}

// ---- events (§4.5) ----

/// <summary>An event from the engine; <see cref="Seq"/> grows by 1 per event on one connection.</summary>
public abstract record DsEvent
{
    public long Seq { get; init; }
}

public sealed record DsAdmissionState
{
    public bool Live { get; init; }

    public long? LeaseUntilMs { get; init; }

    public int Grants { get; init; }

    public int Legs { get; init; }
}

/// <summary><c>world.state</c> (and the <c>world.status</c> reply).</summary>
public sealed record DsWorldState : DsEvent
{
    public required string WorldId { get; init; }

    public bool Running { get; init; }

    /// <summary>Synced peers across engines, distinct; the host's own XUID included when its game is in.</summary>
    public required IReadOnlyList<string> Peers { get; set; }

    public int LobbyPlayers { get; init; }

    /// <summary><c>freeroam</c>, <c>roadtrip</c> or null.</summary>
    public string? Activity { get; init; }

    public required IReadOnlyDictionary<string, int> Engines { get; set; }

    public int Handshaking { get; init; }

    public bool HostInWorld { get; init; }

    public DsAdmissionState? Admission { get; init; }

    public DsNetworkFacts? Network { get; init; }

    public override string ToString() => $"DsWorldState {{ world = {WorldId}, running = {Running}, peers = {Peers.Count}, lobby = {LobbyPlayers} }}";
}

public sealed record DsPeerSynced : DsEvent
{
    public required string WorldId { get; init; }

    public required string Xuid { get; init; }

    public string? Engine { get; init; }

    public int Peer { get; init; }
}

public sealed record DsPeerLeft : DsEvent
{
    public required string WorldId { get; init; }

    public required string Xuid { get; init; }

    public string? Engine { get; init; }

    public int Peer { get; init; }

    public string? Reason { get; init; }
}

public sealed record DsWorldEmpty : DsEvent
{
    public required string WorldId { get; init; }

    public long SinceMs { get; init; }
}

public sealed record DsWorldOccupied : DsEvent
{
    public required string WorldId { get; init; }
}

/// <summary><c>world.closed</c>: <c>controller</c>, <c>shutdown</c>, <c>drained</c>, <c>admission_lapsed</c> or <c>fault</c>.</summary>
public sealed record DsWorldClosed : DsEvent
{
    public required string WorldId { get; init; }

    public required string Reason { get; init; }

    public string? Fault { get; init; }
}

public sealed record DsWorldNetwork : DsEvent
{
    public required DsNetworkFacts Facts { get; init; }
}

/// <summary>An engine request (§4.4): answer with <see cref="DsGameSessionAnswer"/> carrying <see cref="Rid"/>.</summary>
public sealed record DsGameSessionRequest : DsEvent
{
    public required string WorldId { get; init; }

    public required uint Rid { get; init; }

    public required string Kind { get; init; }

    /// <summary>The starter first, then the joiners still connected (1-12); required (absent is malformed).</summary>
    public required IReadOnlyList<string> Xuids { get; set; }
}

public sealed record DsWatchReport : DsEvent
{
    public required string WorldId { get; init; }

    /// <summary>Minted by the engine; posted as <c>PeerObservation.id</c>.</summary>
    public required string Id { get; init; }

    public required string Xuid { get; init; }

    public required string Kind { get; init; }

    public double Value { get; init; }

    public long? SustainMs { get; init; }

    public uint RulesVersion { get; init; }

    public int Peer { get; init; }

    public uint? CarId { get; init; }

    public double? HeldKmh { get; init; }
}

/// <summary><c>peer.rejected</c>: for CrashContext only (XUIDs never reach a log).</summary>
public sealed record DsPeerRejected : DsEvent
{
    public required string WorldId { get; init; }

    public required string Xuid { get; init; }

    public required string Kind { get; init; }

    /// <summary>0-127.</summary>
    public int? Message { get; init; }

    /// <summary>0-15.</summary>
    public int? Router { get; init; }

    /// <summary>u32.</summary>
    public uint? Value { get; init; }

    /// <summary>A stable text id, when the engine has one.</summary>
    public string? Reason { get; init; }

    /// <summary>The contract's ranges (an event outside them is dropped as malformed).</summary>
    [JsonIgnore]
    public bool Valid => Message is null or (>= 0 and <= 127) && Router is null or (>= 0 and <= 15)
        && ulong.TryParse(Xuid, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var xuid) && xuid != 0;

    public override string ToString() => $"DsPeerRejected {{ world = {WorldId}, kind = {Kind} }}";
}

/// <summary><c>diag</c>: counters, kept raw (the launcher logs scalars from it).</summary>
public sealed record DsDiag : DsEvent
{
    public string? WorldId { get; init; }

    public JsonElement Body { get; init; }
}

public sealed record DsNotice : DsEvent
{
    public required string Id { get; init; }

    /// <summary><c>info</c>, <c>warn</c> or <c>error</c>.</summary>
    public string Level { get; set; } = "info";

    public string? WorldId { get; init; }

    public JsonElement? Args { get; init; }
}

public sealed record DsDraining : DsEvent
{
    public long DeadlineMs { get; init; }
}

/// <summary><c>events.dropped</c>: what the engine dropped since the last one (§4.6).</summary>
public sealed record DsEventsDropped : DsEvent
{
    public long Count { get; init; }

    public IReadOnlyDictionary<string, long> Kinds { get; set; } = new Dictionary<string, long>();
}

/// <summary>Not on the wire: the channel closed or the process ended; every world it served is gone (§2.3).</summary>
public sealed record DsGone : DsEvent
{
    public required DsExit Exit { get; init; }

    /// <summary>The worlds that were open (each one needs its <c>DELETE</c> and <c>worlds.host_ended</c>).</summary>
    public required IReadOnlyList<string> Worlds { get; init; }
}

// Properties with a default use `set`, not `init`: the source generator assigns every init-only property, so an absent
// field would become null/default instead of keeping its initializer.
[JsonSourceGenerationOptions(
    PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase,
    DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    RespectNullableAnnotations = true,
    RespectRequiredConstructorParameters = true,
    NumberHandling = JsonNumberHandling.Strict,
    UseStringEnumConverter = false)]
[JsonSerializable(typeof(DsError))]
[JsonSerializable(typeof(DsHello))]
[JsonSerializable(typeof(DsHelloResult))]
[JsonSerializable(typeof(DsKeysSet))]
[JsonSerializable(typeof(DsKeysSetResult))]
[JsonSerializable(typeof(DsBare))]
[JsonSerializable(typeof(DsCapacity))]
[JsonSerializable(typeof(DsHealth))]
[JsonSerializable(typeof(DsDrain))]
[JsonSerializable(typeof(DsEmpty))]
[JsonSerializable(typeof(DsWorldOpen))]
[JsonSerializable(typeof(DsWorldOpenResult))]
[JsonSerializable(typeof(DsWorldContext))]
[JsonSerializable(typeof(DsWorldAdmission))]
[JsonSerializable(typeof(DsAdmissionResult))]
[JsonSerializable(typeof(DsContextResult))]
[JsonSerializable(typeof(DsWorldAdmissionEnd))]
[JsonSerializable(typeof(DsWorldVoice))]
[JsonSerializable(typeof(DsWorldVoiceBlocks))]
[JsonSerializable(typeof(DsVoiceBlocksResult))]
[JsonSerializable(typeof(DsWorldWatch))]
[JsonSerializable(typeof(DsWorldKick))]
[JsonSerializable(typeof(DsKickResult))]
[JsonSerializable(typeof(DsWorldClose))]
[JsonSerializable(typeof(DsWorldStatus))]
[JsonSerializable(typeof(DsGameSessionAnswer))]
[JsonSerializable(typeof(DsWorldState))]
[JsonSerializable(typeof(DsPeerSynced))]
[JsonSerializable(typeof(DsPeerLeft))]
[JsonSerializable(typeof(DsWorldEmpty))]
[JsonSerializable(typeof(DsWorldOccupied))]
[JsonSerializable(typeof(DsWorldClosed))]
[JsonSerializable(typeof(DsNetworkFacts))]
[JsonSerializable(typeof(DsGameSessionRequest))]
[JsonSerializable(typeof(DsWatchReport))]
[JsonSerializable(typeof(DsPeerRejected))]
[JsonSerializable(typeof(DsNotice))]
[JsonSerializable(typeof(DsDraining))]
[JsonSerializable(typeof(DsEventsDropped))]
[JsonSerializable(typeof(DsCatalogSet))]
[JsonSerializable(typeof(DsCatalogSetResult))]
[JsonSerializable(typeof(DsCatalogParts))]
[JsonSerializable(typeof(DsCatalogNeed))]
[JsonSerializable(typeof(JsonElement))]
internal sealed partial class DsJson : JsonSerializerContext;
