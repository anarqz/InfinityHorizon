// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json.Serialization;

namespace Infinity.Core.Matchmaking;

// The wire bodies of /mm/v1 (docs/contracts/MATCHMAKING_API.md §2-§3.4), mirrored from
// crates/fi-protocol/src/matchmaking.rs field for field and held to packages/contracts/src/matchmaking.ts
// by Infinity.Core.Tests ContractParityTests (the TypeScript mirror is fi-protocol's own parity
// target). Never add a field the contract does not have. Wire enums stay strings (as in fi-protocol);
// their values are the constants of MatchmakingValues.cs. Instants are the wire strings.
// `required` members must be in every answer (System.Text.Json refuses a body without them);
// optional ones are nullable and, on request bodies, left out when null (the contract's "absent").

/// <summary>
/// The host of a world, as the list shows it: TypeScript <c>WorldHost | OfficialWorldHost</c>, flattened. A player host is
/// <c>{xuid, gamertag, tier}</c> (<c>tier</c>: <c>free</c> | <c>vip</c>); an official world's (§2.9, OD-188/189) is <c>{official: vmId,
/// name}</c>, with no xuid, gamertag or tier. Members absent from the body stay null and are never written.
/// </summary>
public sealed record WorldHost
{
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public string? Xuid { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public string? Gamertag { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public string? Tier { get; init; }

    /// <summary>§2.9: an official world's Worlds host (its <c>FI_WORLDS</c> vmId); null for a player host.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public string? Official { get; init; }

    /// <summary>§2.9: an official host's display name (the <c>FI_WORLDS</c> name).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public string? Name { get; init; }

    /// <summary>The name shown as the host: an official host's name (or vmId), else the player's gamertag.</summary>
    [JsonIgnore] public string DisplayName => Official is not null ? Name ?? Official : Gamertag ?? string.Empty;
}

/// <summary>A world's common fields (TypeScript <c>World</c>); never an IP address.</summary>
public record World
{
    public required string Id { get; init; }

    public required string Game { get; init; }

    public required string Name { get; init; }

    public required WorldHost Host { get; init; }

    /// <summary>The host's tier: <c>free</c> | <c>supporter</c> (who may join is <see cref="WorldView.Audience"/>).</summary>
    public required string Kind { get; init; }

    public required string Mode { get; init; }

    public required uint Players { get; init; }

    public required uint MaxPlayers { get; init; }

    public string? Region { get; init; }

    /// <summary>One of <see cref="PingClasses"/>; optional in both mirrors (the contract's §2.1 writes it unconditionally).</summary>
    public string? PingClass { get; init; }

    public string? Message { get; init; }

    /// <summary>Deprecated alias of <see cref="WorldView.NeedsCode"/>.</summary>
    public required bool Locked { get; init; }

    public required bool Races { get; init; }

    public required string Since { get; init; }
}

/// <summary>MATCHMAKING_API.md: the lobbies a host's world server can run besides free roam (TypeScript <c>LOBBY_KINDS</c>).</summary>
public static class LobbyKinds
{
    public const string CarMeet = "carmeet";
    public const string RoadTrip = "roadtrip";
}

/// <summary>
/// OD-150 (MATCHMAKING_API.md §2.5): what a world runs now (TypeScript <c>ACTIVITY_KINDS</c>). Only its host sets it
/// (OD-151); a car meet does not change it; <see cref="RoadTrip"/> needs <see cref="LobbyKinds.RoadTrip"/> in the world's lobbies.
/// </summary>
public static class WorldActivities
{
    public const string FreeRoam = "freeroam";
    public const string RoadTrip = "roadtrip";

    /// <summary>CR-S99 (OD-206): an FM5 official world, private or not, runs races; FM5 has no other activity.</summary>
    public const string Race = "race";

    /// <summary>OD-190 (WORLDS_OFFICIAL §11.1): an official world that runs car meets; a player world's activity is never one.</summary>
    public const string CarMeet = "carmeet";

    public static IReadOnlyList<string> All { get; } = [FreeRoam, RoadTrip];

    /// <summary>CR-S99 §1: what a private create may ask for, every game's (FH2 free roam and Road Trip, FM5 race); the server
    /// refuses an activity the ticket's game does not run.</summary>
    public static IReadOnlyList<string> PrivateCreate { get; } = [FreeRoam, RoadTrip, Race];

    /// <summary>fm5-9: the activity a join by world id or code may name, the world's own (FH2 free roam or Road Trip, FM5 race).</summary>
    public static IReadOnlyList<string> Joinable { get; } = [FreeRoam, RoadTrip, Race];

    /// <summary>OD-190 (WORLDS_OFFICIAL §11.2): what a quick join (<c>POST /mm/v1/join {activity}</c>, no world id, no code) asks for.</summary>
    public static IReadOnlyList<string> Placement { get; } = [FreeRoam, CarMeet, RoadTrip];
}

/// <summary>A world as one caller sees it (MATCHMAKING_API.md §2.1).</summary>
public record WorldView : World
{
    /// <summary>One of <see cref="WorldAudiences"/>.</summary>
    public required string Audience { get; init; }

    /// <summary>
    /// OD-141 (CR-S76, §2.8): one of <see cref="WorldModes"/>, the host's mode or <c>any</c> (Hardcore and Noob).
    /// Absent from a server before CR-S76 (null here): read it as <see cref="World.Mode"/> (<see cref="WorldModes.OpenToBoth"/>).
    /// </summary>
    public string? Modes { get; init; }

    public required bool NeedsCode { get; init; }

    public required uint OpenSeats { get; init; }

    /// <summary>The estimated caller → host round trip, rounded to 10 ms; absent when unlocated.</summary>
    public uint? RttMs { get; init; }

    /// <summary>The host's latency limit: 60, 100 or 150 (<see cref="MaxRttSteps"/>).</summary>
    public uint? MaxRttMs { get; init; }

    public required bool CanJoin { get; init; }

    /// <summary>One of <see cref="WorldWhy"/>, only when <see cref="CanJoin"/> is false.</summary>
    public string? Why { get; init; }

    public required bool FriendHost { get; init; }

    /// <summary>
    /// OD-180.2 (S29, CR-S85): the host's game is not in the world and its launcher holds it for the other players.
    /// Omitted when false; null here reads as false.
    /// </summary>
    public bool? HostAway { get; init; }

    /// <summary>§2.9 (OD-188/189): hosted by a Worlds host of <c>FI_WORLDS</c>, not a player (its <see cref="World.Host"/> is official). Omitted when false.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public bool? Official { get; init; }

    /// <summary>
    /// OD-204 (MATCHMAKING_API §2.10, CR-S96): a private official world (created in the game's own menu, friends only). Omitted
    /// elsewhere and by older servers (null reads as false). The client's cue for the private official seat class
    /// (WORLDS_OFFICIAL §12.2 item 11); also on <c>GET /mm/v1/seat</c>'s world.
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public bool? Private { get; init; }

    /// <summary>
    /// OD-204 (MATCHMAKING_API §2.10, CR-S96, main c94d4039): a private official world's effective leader (member XUID), only in
    /// <c>GET /mm/v1/seat</c>'s (and a renew's) world to a reader seated in it; omitted while nobody leads. It may lag one announce
    /// and the seat poll, so the invites route's 409 <c>not_leader</c> still has its notice. T-L4 fills <c>MyWorld.Leader</c> from it.
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public string? Leader { get; init; }

    public required bool Invited { get; init; }

    /// <summary>Only to the host of a <c>friends</c> or <c>private</c> world: 8 digits.</summary>
    public string? Code { get; init; }

    /// <summary>
    /// OD-109: hosted by a dedicated lobby (a launcher with no game). Its host is not a player:
    /// <see cref="World.Players"/> and <see cref="World.MaxPlayers"/> count guests only. Servers without
    /// dedicated support omit it, so it is not <c>required</c> here (absent reads as false).
    /// </summary>
    public bool Dedicated { get; init; }

    /// <summary>
    /// The lobbies the host's world server runs besides free roam (<see cref="LobbyKinds"/>); empty for older hosts, which
    /// therefore never receive a car meet or Road Trip search.
    /// </summary>
    public IReadOnlyList<string> Lobbies { get; init; } = [];

    /// <summary>OD-150: what the world runs now (<see cref="WorldActivities"/>); absent from an older server, read as free roam.</summary>
    public string Activity { get; init; } = WorldActivities.FreeRoam;

    /// <summary>OD-112 (§2.7): the world has a mod set. Absent on older servers (reads as vanilla).</summary>
    public bool Modded { get; init; }

    /// <summary>OD-112: the host's mod set, sorted by id; empty for a vanilla world (and from older servers).</summary>
    public IReadOnlyList<ModEntry> Mods { get; init; } = [];

    /// <summary>OD-112: the players embedded on the list's rows, one world and by-code (≤ 12, host first unless dedicated); absent elsewhere.</summary>
    public IReadOnlyList<ListMember>? Members { get; init; }

    /// <summary>CR-S57 (§5b): <c>relay</c> = the lobby was born on a relay; absent on older servers (reads as direct).</summary>
    public string Transport { get; init; } = Transports.Direct;

    /// <summary>The world's relay, only when <see cref="Transport"/> is <c>relay</c> (shown "via &lt;name&gt;").</summary>
    public WorldRelay? Relay { get; init; }

    /// <summary>
    /// OD-149 (§3.9): a pineapple world, hosted by a profile the anti-cheat system isolated, for the world's whole life; only
    /// isolated profiles join it (a clean caller reads <c>canJoin: false, why: "pool"</c>). Absent on older servers (false).
    /// </summary>
    public bool Pool { get; init; }

    /// <summary>
    /// CR-S67 (§2.5): the lobby's voice setting, one of <see cref="VoiceModes"/> (display and the host's own settings only; playback
    /// follows the host relay's state down). Absent on older servers: read as <see cref="VoiceModes.Open"/>.
    /// </summary>
    public string Voice { get; init; } = VoiceModes.Open;
}

/// <summary>CR-S67 (MATCHMAKING_API.md §2.5, TypeScript <c>VOICE_MODES</c>): a lobby's voice chat setting.</summary>
public static class VoiceModes
{
    public const string Off = "off", Open = "open", Proximity = "proximity";

    public static IReadOnlyList<string> All { get; } = [Off, Open, Proximity];
}

/// <summary>
/// OD-149 (§3.9): <c>pool: {until}</c> on <c>GET /mm/v1/worlds</c> and <c>GET /mm/v1/seat</c> for a caller the anti-cheat system
/// isolated (absent otherwise): until <see cref="Until"/> (a server instant) it plays only in pineapple worlds.
/// </summary>
public sealed record CallerPool
{
    public required string Until { get; init; }
}

/// <summary>CR-S57: how a pair's datagrams travel.</summary>
public static class Transports
{
    public const string Direct = "direct", Relay = "relay";
}

/// <summary>CR-S57: a launcher's NAT test.</summary>
public static class NatTypes
{
    public const string Open = "open", Restricted = "restricted", Symmetric = "symmetric";
}

/// <summary>A world's relay (CR-S57): its announce key and name (untrusted display text).</summary>
public sealed record WorldRelay
{
    public required string PublicKey { get; init; }

    public required string Name { get; init; }
}

/// <summary>CR-S57 signed <c>relay</c> claim: the relay both sides meet at (its key, host, binding and rendezvous ports).</summary>
public sealed record RelayGrantRelay
{
    [JsonPropertyName("key")] public required string Key { get; init; }
    [JsonPropertyName("addr")] public required string Addr { get; init; }
    [JsonPropertyName("binding")] public required ushort Binding { get; init; }
    [JsonPropertyName("port")] public required ushort Port { get; init; }
}

/// <summary>
/// One mod of a mod set (OD-112, MATCHMAKING_API.md §2.7): <c>id</c> the manifest's multiplayer
/// <c>compatibilityId</c>, <c>version</c> its revision (decimal), <c>fingerprint</c> 64 lowercase hex
/// digits (<c>infinity-mod-compatibility-v1</c>). A set is sorted by id, unique, at most 32.
/// </summary>
public sealed record ModEntry
{
    public required string Id { get; init; }

    public required string Version { get; init; }

    public required string Fingerprint { get; init; }
}

/// <summary>A player embedded in a listed world (OD-112): no XUID, no relation (<c>tier</c> free | vip; <c>role</c> host | seated | reserved).</summary>
public sealed record ListMember
{
    public required string Gamertag { get; init; }

    public required string Tier { get; init; }

    public required string Role { get; init; }

    /// <summary>
    /// CR-S84 (OD-181, §2.7): the set this player declared (the host's row: the world's set); absent, null or empty: none.
    /// An older server sends none.
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<ModEntry>? Mods { get; init; }
}

/// <summary>
/// <c>not_allowed {reason: "mods"}</c>: the world's set (<c>expected</c>) and the caller's (<c>actual</c>). Retired by OD-181
/// (CR-S84); still read so an older server's refusal keeps its message.
/// </summary>
public sealed record ModsMismatch
{
    public required IReadOnlyList<ModEntry> Expected { get; init; }

    public required IReadOnlyList<ModEntry> Actual { get; init; }
}

/// <summary><c>POST /mm/v1/worlds/{worldId}/requests</c> optional body (OD-112; absent: vanilla).</summary>
public sealed record RequestToJoin
{
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<ModEntry>? Mods { get; init; }

    /// <summary>
    /// OD-160 (§3.4, §5b "Relay by mode"): the asker's measured relay round trips, same rules as <see cref="JoinRequest.Relays"/>, so an
    /// acceptance into a relay-born world seats it on the world's relay (without it: 409 <c>launcher_update_required</c> there).
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyDictionary<string, int>? Relays { get; init; }
}

public sealed record ListViewer
{
    public required bool Located { get; init; }

    public string? Region { get; init; }
}

/// <summary><c>GET /mm/v1/worlds?filter=&amp;notFull=</c> → 200 (OD-58: at most 5 items and 3 locked).</summary>
public sealed record WorldList
{
    public required string Filter { get; init; }

    public required IReadOnlyList<WorldView> Items { get; init; }

    /// <summary>Always empty since OD-129; an older server listed up to 3 Supporter-only worlds here for a free ticket (<c>why: "tier"</c>).</summary>
    public required IReadOnlyList<WorldView> Locked { get; init; }

    public required string GeneratedAt { get; init; }

    public required ListViewer Viewer { get; init; }

    /// <summary>OD-149 (§3.9): the caller is isolated by the anti-cheat system until then; absent otherwise (and from older servers).</summary>
    public CallerPool? Pool { get; init; }
}

/// <summary>The host's relay UDP port and up to 4 private LAN addresses (TypeScript <c>AnnouncePort</c>).</summary>
public sealed record AnnouncePort
{
    public required ushort Port { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<string>? Lan { get; init; }
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public AnnounceExternalPort? External { get; init; }
}

public sealed record AnnounceExternalPort
{
    public required ushort Port { get; init; }
    public required string Source { get; init; }
}

/// <summary><c>PUT /mm/v1/worlds/{worldId}</c> body (§2.5): open or change the host's world.</summary>
public sealed record WorldSettings
{
    public required string Name { get; init; }

    /// <summary>OD-146 (§5b): <c>relay</c> makes a Noob world relay-born. OD-160 removed the launcher's switch: this client never sets it.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Transport { get; init; }

    public required string Mode { get; init; }

    /// <summary>
    /// OD-141 (CR-S76, §2.8): the host's own mode or <c>any</c> (<see cref="WorldModes"/>); null: left out (the server
    /// takes the host's mode). Fixed once the world opens: another value is 409 <c>PORTAL.CONFLICT {reason: "modes"}</c>;
    /// the other mode alone is 409 <c>PORTAL.MODE_MISMATCH</c>.
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Modes { get; init; }

    public required uint MaxPlayers { get; init; }

    public required string Audience { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public uint? MaxRttMs { get; init; }

    public required bool Races { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Message { get; init; }

    /// <summary>The XUIDs that may join (≤ 256), for every host (OD-129).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<string>? Allow { get; init; }

    public required AnnouncePort Endpoint { get; init; }

    /// <summary>OD-112: the host's mod set (null: vanilla, left out); fixed once the world opens (another set is 409 <c>PORTAL.CONFLICT {reason: "mods"}</c>).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<ModEntry>? Mods { get; init; }

    /// <summary>The lobbies this host's world server runs besides free roam (<see cref="LobbyKinds"/>, distinct); null: left out.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<string>? Lobbies { get; init; }

    /// <summary>OD-150: what the world runs now (<see cref="WorldActivities"/>); null: left out (unchanged; a new world is free roam).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Activity { get; init; }

    /// <summary>CR-S57: the host's NAT test, read when the world opens (<see cref="NatTypes"/>).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Nat { get; init; }

    /// <summary>CR-S57: measured relay round trips (announce key → ms, ≤ 16, 0-5000).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyDictionary<string, int>? Relays { get; init; }

    /// <summary>CR-S67: the lobby's voice setting (<see cref="VoiceModes"/>); null: left out (unchanged; a new world is open).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Voice { get; init; }
}

/// <summary><c>POST /mm/v1/worlds/{worldId}/announce</c> body, every 15 s: <c>peers</c> are who is in the world now (≤ 11).</summary>
public sealed record Heartbeat
{
    public required IReadOnlyList<string> Peers { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public AnnouncePort? Endpoint { get; init; }

    /// <summary>OD-150: the host's game changed activity (<see cref="WorldActivities"/>); null: left out (unchanged).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Activity { get; init; }

    /// <summary>CR-S67: the host changed the lobby's voice setting (<see cref="VoiceModes"/>); null: left out (unchanged).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Voice { get; init; }

    /// <summary>
    /// OD-180.2 (S29, CR-S85): the host's game is away and its launcher holds the world for the other players. Absent means
    /// false on every heartbeat; null here is never written (a server before CR-S85 refuses the field).
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public bool? HostAway { get; init; }

    /// <summary>
    /// OD-194 (CR-S91), official worlds only (their world agent sends it): past the point newcomers may join. Absent means false;
    /// a player host's launcher never writes it (null).
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public bool? InProgress { get; init; }

    /// <summary>
    /// OD-204 (MATCHMAKING_API §2.10, WORLDS_OFFICIAL §12.8b): a private official world's current leader (member XUID), sent by the
    /// world agent only, when someone leads. A player host's launcher never writes it (null: left out; an older server refuses the
    /// member as unknown). The DTO member mirrors the contract (T-L4); this client sends none.
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Leader { get; init; }
}

/// <summary>The host's answer to <c>PUT</c> and the announce: the world as its host sees it (<c>code</c> included) and the punches.</summary>
public sealed record WorldAnnounced : WorldView
{
    public required IReadOnlyList<Punch> Punches { get; init; }
}

/// <summary>Base64 of the canonical claims and the base64 Ed25519 signature over <c>"fi-relay-grant-v1\n"</c> ‖ claims.</summary>
public sealed record RelayGrant
{
    public required string Claims { get; init; }

    public required string Signature { get; init; }
    public RelayUdpProof? UdpProof { get; init; }
    public override string ToString() => "RelayGrant { redacted }";
}

/// <summary>Grant-scoped TLS-only secret; never include in state, logging or UDP.</summary>
public sealed record RelayUdpProof
{
    public required uint Version { get; init; }
    public required string Key { get; init; }
    public override string ToString() => "RelayUdpProof { redacted }";
    internal byte[]? Decode()
    {
        if (Version != 1 || Key is not { Length: 44 }) return null;
        try { var bytes = Convert.FromBase64String(Key); return bytes.Length == 32 && Convert.ToBase64String(bytes) == Key ? bytes : null; }
        catch (FormatException) { return null; }
    }
}

public sealed record PeerEndpoint
{
    public required string Ip { get; init; }

    public required ushort Port { get; init; }
}

/// <summary>One grant the host's relay must allow, or drop (<c>kicked</c>).</summary>
public sealed record Punch
{
    public NetworkHints? Candidates { get; init; }
    public required string GrantId { get; init; }

    public required string Xuid { get; init; }

    public PeerEndpoint? Endpoint { get; init; }

    public required RelayGrant Relay { get; init; }

    public required bool Kicked { get; init; }

    /// <summary>CR-S57: <c>relay</c> = hold a relay leg for this grant; absent on older servers (direct).</summary>
    public string Transport { get; init; } = Transports.Direct;
}

/// <summary>Existing GET /mm/v1/punches response (fi_protocol::matchmaking::Punches).</summary>
public sealed record Punches
{
    public required IReadOnlyList<Punch> Items { get; init; }
}

/// <summary><c>POST /mm/v1/worlds/{worldId}/moderation</c> body (<c>until</c>: <c>ban</c> only).</summary>
public sealed record Moderation
{
    /// <summary>One of <see cref="ModerationActions"/>.</summary>
    public required string Action { get; init; }

    public required string Xuid { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Until { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Reason { get; init; }
}

public sealed record ModerationResult
{
    public required string Action { get; init; }

    public required string Xuid { get; init; }

    public string? Until { get; init; }
}

/// <summary><c>POST /mm/v1/join</c> body: a world id, a code (8 digits), or both; or neither with an activity (OD-190's quick join).</summary>
public sealed record JoinRequest
{
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? WorldId { get; init; }

    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Code { get; init; }

    /// <summary>OD-112: the caller's mod set (null: vanilla, left out; every launcher up to 1.0.8 sends none).</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<ModEntry>? Mods { get; init; }

    /// <summary>CR-S57: the joiner's NAT test.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Nat { get; init; }

    /// <summary>CR-S57: the joiner's measured relay round trips.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyDictionary<string, int>? Relays { get; init; }

    /// <summary>OD-146 (§5b): <c>relay</c> asks for a relay-born seat. OD-160 removed the launcher's switch: this client never sets it.</summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Transport { get; init; }

    /// <summary>
    /// OD-150: the activity asked for (<see cref="WorldActivities"/>); a world running another is 409 <c>{reason: "activity"}</c>; null: any.
    /// OD-190: alone (no world id, no code) it is a quick join into an official world (<see cref="WorldActivities.Placement"/>).
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Activity { get; init; }

    /// <summary>
    /// OD-204 (MATCHMAKING_API §2.10, CR-S96): <c>true</c> with an <see cref="Activity"/> (<c>freeroam</c> or <c>roadtrip</c>) and no
    /// world id or code is the private create: the caller's own private official world, or a spare promoted for it. Null: left out
    /// (every other join; an older server refuses the member as unknown).
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public bool? Private { get; init; }

    /// <summary>
    /// OD-205 (MATCHMAKING_API §2.10): the caller's region, a declared <c>FI_WORLDS</c> region; the quick join and the private create
    /// filter by it. T-L4: the DTO member only; this client NEVER sets it yet (a server without a region configuration answers
    /// 400 <c>REQUEST.INVALID {field: "region"}</c>), so it stays null and is left out.
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Region { get; init; }
}

/// <summary>A reserved seat (<c>expiresAt</c> ≤ 60 s away, renewable up to <c>holdUntil</c>).</summary>
public sealed record Grant
{
    public required string Id { get; init; }

    public required string WorldId { get; init; }

    public required string ExpiresAt { get; init; }

    public required string HoldUntil { get; init; }

    public required RelayGrant Relay { get; init; }

    /// <summary>CR-S57: the joiner's expected transport now; absent on older servers (direct).</summary>
    public string Transport { get; init; } = Transports.Direct;
}

/// <summary>Seats of a full world.</summary>
public sealed record WorldSeats
{
    public required uint Players { get; init; }

    public required uint MaxPlayers { get; init; }
}

/// <summary>The live seat that locks the caller (follow it, or leave it with <c>DELETE /mm/v1/seat</c>).</summary>
public sealed record LockedSeat
{
    public required string WorldId { get; init; }

    public required string GrantId { get; init; }

    public required string ExpiresAt { get; init; }
}

/// <summary>With <c>not_allowed {reason: "latency"}</c>.</summary>
public sealed record LatencyRefusal
{
    public required uint RttMs { get; init; }

    public required uint MaxRttMs { get; init; }
}

/// <summary>
/// <c>POST /mm/v1/join</c> → 200, or 429 with <c>status: "rate_limited"</c>. One shape for every
/// status (as both mirrors have it): <c>granted</c> → <see cref="Grant"/>, <c>full</c> →
/// <see cref="World"/>, <c>locked</c> → <see cref="Locked"/>, <c>not_allowed</c> →
/// <see cref="Reason"/> (and <see cref="Latency"/>), <c>rate_limited</c> → <see cref="RetryAfterSeconds"/>.
/// </summary>
public sealed record JoinAnswer
{
    /// <summary>One of <see cref="JoinStatuses"/>.</summary>
    public required string Status { get; init; }

    public Grant? Grant { get; init; }

    public WorldSeats? World { get; init; }

    public LockedSeat? Locked { get; init; }

    /// <summary>One of <see cref="NotAllowedReasons"/>.</summary>
    public string? Reason { get; init; }

    public LatencyRefusal? Latency { get; init; }

    /// <summary>With <c>not_allowed {reason: "mods"}</c> (OD-112); retired by OD-181 (CR-S84), read only from an older server.</summary>
    public ModsMismatch? Details { get; init; }

    public uint? RetryAfterSeconds { get; init; }
}

/// <summary>The caller's live seat, or the world it hosts (<c>state</c>: one of <see cref="SeatStates"/>).</summary>
public sealed record Seat
{
    public required string State { get; init; }

    public required string WorldId { get; init; }

    public required WorldView World { get; init; }

    /// <summary>Reserved and seated.</summary>
    public string? GrantId { get; init; }

    /// <summary>Reserved.</summary>
    public string? ExpiresAt { get; init; }

    /// <summary>Reserved.</summary>
    public string? HoldUntil { get; init; }

    /// <summary>Reserved and seated: role <c>joiner</c>, fresh (≤ 60 s).</summary>
    public RelayGrant? Relay { get; init; }

    /// <summary>CR-S57, reserved and seated: the joiner's expected transport now.</summary>
    public string? Transport { get; init; }
}

/// <summary>How the caller's last seat ended (<c>reason</c>: one of <see cref="SeatEnds"/>).</summary>
public sealed record SeatEnded
{
    public required string WorldId { get; init; }

    public required string GrantId { get; init; }

    public required string At { get; init; }

    public required string Reason { get; init; }
}

/// <summary>The caller's account's latest request to join (pending, or answered within two minutes).</summary>
public sealed record SeatRequest
{
    public required string Id { get; init; }

    public required string WorldId { get; init; }

    /// <summary>One of <see cref="OfferStates"/>.</summary>
    public required string State { get; init; }

    public required string ExpiresAt { get; init; }
}

/// <summary><c>GET /mm/v1/seat</c> and <c>POST /mm/v1/seat/renew</c> → 200.</summary>
public sealed record SeatAnswer
{
    /// <summary>The live seat; <c>null</c> on the wire when none.</summary>
    public required Seat? Seat { get; init; }

    public SeatEnded? Ended { get; init; }

    public SeatRequest? Request { get; init; }

    /// <summary>Opt-in atomic roster from GET seat?includeMembers=true; absent on older servers/no seat.</summary>
    public WorldMembers? Members { get; init; }

    public required string GeneratedAt { get; init; }

    /// <summary>OD-149 (§3.9): the caller is isolated by the anti-cheat system until then; absent otherwise (and from older servers).</summary>
    public CallerPool? Pool { get; init; }

    /// <summary>
    /// OD-204 (MATCHMAKING_API §2.10, WORLDS_OFFICIAL §12.6 "Crash or reconnect"): the open private official world this account may
    /// rejoin (a crash, a restart); absent: none, or an older server. T-L4: a rejoin asks it first by world id.
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? PrivateWorldId { get; init; }
}

/// <summary><c>POST /mm/v1/seat/renew</c> body.</summary>
public sealed record SeatRenew
{
    public required string GrantId { get; init; }

    /// <summary>
    /// CR-S57: <c>relay</c> once the joiner's punch failed and it moved to the grant's relay. OD-160: <c>direct</c> once a relay-born
    /// seat could not reach its relay and plays direct with the host instead.
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public string? Transport { get; init; }
}

/// <summary><see cref="GameModeSessionRequest.Kind"/>: <c>freeroam</c> only for now.</summary>
public static class GameModeSessionKinds
{
    public const string FreeRoam = "freeroam";
}

/// <summary>
/// <c>POST /mm/v1/worlds/{worldId}/game-sessions</c> body (§3.8): the shared session a car meet invitation takes its joiners to.
/// <c>xuids</c>: decimal, 1-12, distinct; the host or players seated in this world.
/// </summary>
public sealed record GameModeSessionRequest
{
    public const int MaxMembers = 12;
    public required string Kind { get; init; }
    public required IReadOnlyList<string> Xuids { get; init; }
}

/// <summary>201 of <c>…/game-sessions</c>: the session every joiner is sent to (<c>name</c> and <c>templateName</c>).</summary>
public sealed record GameModeSession
{
    public required string Kind { get; init; }
    public required string Scid { get; init; }
    public required string TemplateName { get; init; }
    public required string Name { get; init; }
    public required IReadOnlyList<string> Members { get; init; }
}

/// <summary><c>POST /mm/v1/worlds/{worldId}/invites</c> body: any XUID of the friend account (a handle works).</summary>
public sealed record InviteRequest
{
    public required string Xuid { get; init; }
}

/// <summary>
/// OD-210 (CR-S101, MATCHMAKING_API §2.11; TypeScript <c>WorldJoinModes</c>): <c>PUT /mm/v1/worlds/{worldId}/modes</c>, who may join
/// a private official world from now on: one of <see cref="WorldModes"/> (<c>any</c>: Everyone). Seats already granted stay.
/// </summary>
public sealed record WorldJoinModes
{
    public required string Modes { get; init; }
}

/// <summary>An invite (<c>…/invites</c>) or a request to join (<c>…/requests</c>): 201, 200 for the pending one.</summary>
public sealed record Invite
{
    public required string Id { get; init; }

    /// <summary>One of <see cref="OfferKinds"/>.</summary>
    public required string Kind { get; init; }

    public required string WorldId { get; init; }

    /// <summary>The handle of the account that answers: the invitee, or the host.</summary>
    public required string To { get; init; }

    /// <summary>One of <see cref="OfferStates"/>.</summary>
    public required string State { get; init; }

    public required string CreatedAt { get; init; }

    public required string ExpiresAt { get; init; }

    /// <summary><c>accepted</c>: the seat it gave.</summary>
    public string? GrantId { get; init; }
}

/// <summary>
/// One player of a world, as its host or a seat holder sees it (CR-L25, OD-63:
/// <c>GET /mm/v1/worlds/{worldId}/members</c>; the host, then seated by arrival, then reserved;
/// no addresses, no account ids). The XUID is an action key only, never shown (OD-61).
/// </summary>
public sealed record WorldMember
{
    public required string Xuid { get; init; }

    public required string Gamertag { get; init; }

    /// <summary><c>free</c> or <c>vip</c>.</summary>
    public required string Tier { get; init; }

    /// <summary><c>host</c>, <c>seated</c> or <c>reserved</c>.</summary>
    public required string Role { get; init; }

    /// <summary>To the caller's account (OD-55): <c>none</c>, <c>friends</c>, <c>outgoing</c>, <c>incoming</c> or <c>self</c>.</summary>
    public string? Friend { get; init; }

    public string? Since { get; init; }

    /// <summary>
    /// CR-S67 (§3.7): the member's account's voice block in force at the read (a wire instant), else null; the host's row
    /// included. Never why or who muted. Absent on older servers (null).
    /// </summary>
    public string? VoiceBlockedUntil { get; init; }

    /// <summary>
    /// CR-S84 (OD-181, §3.7): the set this member declared (the host's row: the world's set; a seat's: its join's); absent,
    /// null or empty: none. An older server sends none.
    /// </summary>
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public IReadOnlyList<ModEntry>? Mods { get; init; }
}

/// <summary>CR-L25 (OD-63) → 200.</summary>
public sealed record WorldMembers
{
    public string? WorldId { get; init; }

    public required IReadOnlyList<WorldMember> Items { get; init; }

    public string? GeneratedAt { get; init; }
}
