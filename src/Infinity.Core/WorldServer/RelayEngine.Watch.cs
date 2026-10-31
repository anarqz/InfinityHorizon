// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;

namespace Infinity.Core.WorldServer;

/// <summary>
/// The watch roster (AC-2, MATCHMAKING_API §5e, docs/launcher/ANTICHEAT.md): a STUN indication from a Hardcore world's server to each joiner's hook
/// proxy, naming the world server's peer index of every other player with its verified XUID, so the joiner's hook can say whose
/// car it measured (a relayed car prediction carries only the owner's index). <c>{u8 1, u8 count 0-12, count x {u8 index,
/// u64 BE xuid}}</c>, padded to 4 bytes, in attribute 0xC0F7 of kind 0x3E95, on every change and every 30 s. An older hook
/// drops the kind unread.
/// </summary>
internal static class WatchRosterCodec
{
    internal const ushort Kind = 0x3e95;
    internal const ushort Attribute = 0xc0f7; // 0xC0F5/0xC0F6 are the voice control (§5c)
    internal const int MaxEntries = 12;

    internal static byte[] Write(IReadOnlyList<(byte Index, ulong Xuid)> entries, ReadOnlySpan<byte> transaction = default)
    {
        if (entries.Count > MaxEntries) throw new WireFormatException(WireError.LengthLimit);
        var value = new byte[2 + 9 * entries.Count];
        value[0] = 1; value[1] = (byte)entries.Count;
        for (var i = 0; i < entries.Count; i++)
        {
            value[2 + 9 * i] = entries[i].Index;
            BinaryPrimitives.WriteUInt64BigEndian(value.AsSpan(3 + 9 * i), entries[i].Xuid);
        }
        return StunCodec.Indication(Kind, Attribute, value, transaction);
    }

    internal static IReadOnlyList<(byte Index, ulong Xuid)>? Parse(ReadOnlySpan<byte> data)
    {
        if (!StunCodec.TryAttribute(data, Kind, Attribute, out var value) || value.Length < 2 || value[0] != 1 || value[1] > MaxEntries
            || value.Length != 2 + 9 * value[1]) return null;
        var entries = new List<(byte, ulong)>(value[1]);
        for (var i = 0; i < value[1]; i++) entries.Add((value[2 + 9 * i], BinaryPrimitives.ReadUInt64BigEndian(value[(3 + 9 * i)..])));
        return entries;
    }
}

/// <summary>
/// AC-2: the host measures every other player's car in its world server (MATCHMAKING_API §3.9). Only while the launcher set
/// rules (a Hardcore world not open to both modes); never the host's own car (its XUID, or a loopback peer).
/// </summary>
internal sealed partial class RelayEngine
{
    internal const long WatchRosterMs = 30000;
    private PeerWatch? _watch;
    private ulong _watchSelf;
    private long _watchRosterAt = long.MinValue;
    private string _watchRosterSent = string.Empty;

    /// <summary>
    /// The rules to measure against (null: stop; every episode in flight ends now), the host's own XUID (never measured), and
    /// where each ended episode goes.
    /// </summary>
    internal void SetWatch(PeerWatchRules? rules, ulong self, Action<PeerWatchReport>? report, long nowMilliseconds)
    {
        _now = Math.Max(_now, nowMilliseconds);
        _watch?.Flush(_now);
        _parts?.Flush();
        _watch = rules is not null && report is not null ? new PeerWatch(rules, report, Note) : null;
        _parts = _watch is not null && rules!.Parts is { } catalog ? new PartsWatch(catalog, rules.Version, report!, Note) : null;
        _watchSelf = self;
        _watchRosterAt = long.MinValue; _watchRosterSent = string.Empty;
        if (_watch is not null) Note($"watch: on (rules version {rules!.Version}); the other players' cars are measured{(_parts is null ? "; no parts rule (no readable game database)" : ", their parts too")}");
    }

    /// <summary>NR-6: the parts rule, while watching with a readable game database.</summary>
    private PartsWatch? _parts;

    /// <summary>NR-6: a peer's own player object (router 1, sanitised) as it arrived: its model and part rows, when readable.</summary>
    private void WatchPlayerObject(RelayPeer peer, bool create, BitBuffer payload)
    {
        if (_parts is not { } parts || !Watched(peer)) return;
        var (model, rows) = PlayerParts.Read(payload);
        if (model is null && rows is null && !create) return;
        parts.Observe(peer.Index, peer.Xuid, model, rows, fresh: create);
    }

    internal bool Watching => _watch is not null;

    private bool Watched(RelayPeer peer) => peer.Xuid != _watchSelf && !SharedXuid(peer.Xuid) && !IsLoopback(peer.Endpoint.Address);

    /// <summary>A peer's own car prediction, as it arrived (its position already read and bounded).</summary>
    private void WatchCar(RelayPeer peer, NerdMessage.Car car)
    {
        if (_watch is not { } watch || !peer.Synced || !Watched(peer) || peer.CarPosition is not { } position) return;
        var reader = new BitReader(car.Body.Bytes, car.Body.Length);
        reader.Skip(PeerWatch.VelocityOffset);
        var velocity = PeerWatch.Velocity(unchecked((short)reader.Read(16)), unchecked((short)reader.Read(16)), unchecked((short)reader.Read(16)));
        reader.Skip(CarUniverseOffset - PeerWatch.VelocityOffset - 48);
        var universe = (byte)reader.Read(PrivateUniverseBits);
        reader.Skip(CarStampOffset - CarUniverseOffset - PrivateUniverseBits);
        var stamp = (uint)reader.Read(32);
        watch.Sample(peer.Index, peer.Xuid, position, velocity, stamp, universe, _now);
    }

    private void WatchEventStart(string why) => _watch?.Exclude(why);

    private void WatchPeerLeft(RelayPeer peer)
    {
        _watch?.Forget(peer.Index, _now);
        _parts?.Forget(peer.Index);
        _watchRosterAt = long.MinValue;
    }

    private void TickWatch()
    {
        if (_watch is not { } watch) return;
        watch.Tick(_now);
        var synced = _peers.Where(p => p.Synced).ToArray();
        var key = string.Join(',', synced.Select(p => p.Index + ":" + p.Xuid));
        if (key == _watchRosterSent && _now - _watchRosterAt < WatchRosterMs) return;
        _watchRosterSent = key; _watchRosterAt = _now;
        // only to a joiner whose hook proved this very socket (its proxy, which drops the kind before the game)
        if (AuthorizeHook is null) return;
        foreach (var receiver in synced)
        {
            if (IsLoopback(receiver.Endpoint.Address) || !Allowed(receiver.Xuid, receiver.Endpoint) || !AllowedHook(receiver.Xuid, receiver.Endpoint)) continue;
            var others = synced.Where(p => p != receiver && !SharedXuid(p.Xuid)).Take(WatchRosterCodec.MaxEntries).Select(p => (p.Index, p.Xuid)).ToArray();
            Emit(receiver.Endpoint, WatchRosterCodec.Write(others));
        }
    }
}
