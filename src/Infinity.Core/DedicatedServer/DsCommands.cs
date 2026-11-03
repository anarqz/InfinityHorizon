// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Matchmaking;

namespace Infinity.Core.DedicatedServer;

/// <summary>
/// Builds the control commands from what the launcher already holds (DEDICATED_SERVER.md §4.3), so the stage 2 wiring
/// in <c>WorldsBridge</c> converts exactly as the in-process relay does today.
/// </summary>
public static class DsCommands
{
    /// <summary>At most this many roster rows per <c>world.voiceBlocks</c> (§4.3).</summary>
    public const int MaxVoiceBlocks = 64;

    /// <summary>At most this many punches per snapshot (§6.2; more is <c>context_invalid</c>, fail closed).</summary>
    public const int MaxPunches = 256;

    /// <summary>
    /// A PUT or announce/heartbeat answer (<c>UpdateWorldAsync</c> today): <c>world.context</c> (the context and the host
    /// lease, never punches), then <c>world.admission</c> with the answer's punches verbatim, in that order. The engine
    /// refuses more than <see cref="MaxPunches"/>.
    /// </summary>
    public static (DsWorldContext Context, DsWorldAdmission Admission) Announced(WorldAnnounced world, DateTimeOffset serverTime)
    {
        ArgumentNullException.ThrowIfNull(world);
        return (Context(world, serverTime), new DsWorldAdmission { WorldId = world.Id, ServerTimeMs = serverTime.ToUnixTimeMilliseconds(), Punches = world.Punches });
    }

    /// <summary><c>world.context</c>: the world, its host and the trusted time; renews the 45 s host lease.</summary>
    public static DsWorldContext Context(World world, DateTimeOffset serverTime)
    {
        ArgumentNullException.ThrowIfNull(world);
        return new DsWorldContext
        {
            WorldId = world.Id,
            ServerTimeMs = serverTime.ToUnixTimeMilliseconds(),
            WorldInfo = new DsContextWorld { Id = world.Id, Game = world.Game, Host = new DsContextHost { Xuid = world.Host.Xuid } },
        };
    }

    /// <summary><c>world.admission</c> from a <c>GET /mm/v1/punches</c> read (<c>RefreshAdmissionsAsync</c> today).</summary>
    public static DsWorldAdmission Admission(string worldId, Punches punches, DateTimeOffset serverTime)
    {
        ArgumentNullException.ThrowIfNull(punches);
        return new DsWorldAdmission { WorldId = worldId, ServerTimeMs = serverTime.ToUnixTimeMilliseconds(), Punches = punches.Items };
    }

    /// <summary>
    /// <c>world.voiceBlocks</c> from the host's roster poll (<c>SetRosterVoiceBlocks</c> today): null → 0, unreadable → row
    /// dropped, a fractional second rounded up; XUIDs that do not parse (<c>NumberStyles.None</c>) or are 0 are left out.
    /// </summary>
    public static DsWorldVoiceBlocks VoiceBlocks(string worldId, IEnumerable<WorldMember> members)
    {
        ArgumentNullException.ThrowIfNull(members);
        var blocks = new Dictionary<string, long>(StringComparer.Ordinal);
        foreach (var member in members)
        {
            if (blocks.Count == MaxVoiceBlocks)
            {
                break;
            }
            if (member is null || !ulong.TryParse(member.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid) || xuid == 0)
            {
                continue;
            }
            if (IntegratedWorldRelay.VoiceBlockSeconds(member.VoiceBlockedUntil) is { } until)
            {
                blocks[xuid.ToString(CultureInfo.InvariantCulture)] = until;
            }
        }
        return new DsWorldVoiceBlocks { WorldId = worldId, Blocks = blocks };
    }

    /// <summary><c>world.voice</c> from the overlay's choice (<c>SetVoice</c> today); the mode is strict (§14 item 4).</summary>
    public static DsWorldVoice Voice(string worldId, string mode, IEnumerable<ulong> mutedForLobby)
    {
        ArgumentNullException.ThrowIfNull(mutedForLobby);
        if (mode is not ("off" or "open" or "proximity"))
        {
            throw new ArgumentOutOfRangeException(nameof(mode), mode, "off, open or proximity");
        }
        return new DsWorldVoice
        {
            WorldId = worldId,
            Mode = mode,
            LobbyMuted = mutedForLobby.Distinct().Take(MaxVoiceBlocks).Select(x => x.ToString(CultureInfo.InvariantCulture)).ToArray(),
        };
    }

    /// <summary><c>world.watch</c> (<c>Watch(rules, self, report)</c> today); <paramref name="self"/> only for a player host.</summary>
    public static DsWorldWatch Watch(string worldId, WatchRules? rules, ulong? self, bool parts, bool forceSpeed = false) => new()
    {
        WorldId = worldId,
        Rules = rules,
        Self = self is { } s and not 0 ? s.ToString(CultureInfo.InvariantCulture) : null,
        Parts = parts,
        ForceSpeed = forceSpeed ? true : null,
    };

    /// <summary>The heartbeat's <c>peers</c> from <c>world.state.peers</c> (§4.5): minus the host's own XUID, never truncated.</summary>
    public static IReadOnlyList<string> HeartbeatPeers(DsWorldState state, string hostXuid)
    {
        ArgumentNullException.ThrowIfNull(state);
        return (state.Peers ?? []).Where(p => p is not null && !string.Equals(p, hostXuid, StringComparison.Ordinal)).Distinct(StringComparer.Ordinal).ToArray();
    }
}
