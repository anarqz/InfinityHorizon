// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Net;
using System.Net.Sockets;

namespace Infinity.Core.WorldServer;

internal sealed partial class RelayEngine
{
    // The carstate channel only. OD-66's livery channel (0x3E92, MATCHMAKING_API.md 5a) is retired
    // by CR-S59 (the server keeps the liveries): an older hook's livery messages are dropped unread.
    internal const int SideChannelPacketsPerSecond = 4096;
    internal const int SideChannelBytesPerSecond = 2 * 1024 * 1024;
    internal const int SideChannelControlsPerSecond = 128;
    private sealed class SideChannelRate(long now)
    {
        internal long At { get; set; } = now;
        internal double Packets { get; set; } = SideChannelPacketsPerSecond * 2.0;
        internal double Bytes { get; set; } = SideChannelBytesPerSecond * 2.0;
        internal double Controls { get; set; } = SideChannelControlsPerSecond * 2.0;
    }
    private readonly Dictionary<RelayPeer, SideChannelRate> _sideChannelRates = [];
    internal long SideChannelRateDrops { get; private set; }

    private bool TakeSideChannelRate(RelayPeer peer, int bytes, bool control)
    {
        if (!_sideChannelRates.TryGetValue(peer, out var rate)) _sideChannelRates[peer] = rate = new(_now);
        var elapsed = Math.Max(0, _now - rate.At) / 1000.0; rate.At = _now;
        rate.Packets = Math.Min(SideChannelPacketsPerSecond * 2.0, rate.Packets + elapsed * SideChannelPacketsPerSecond);
        rate.Bytes = Math.Min(SideChannelBytesPerSecond * 2.0, rate.Bytes + elapsed * SideChannelBytesPerSecond);
        rate.Controls = Math.Min(SideChannelControlsPerSecond * 2.0, rate.Controls + elapsed * SideChannelControlsPerSecond);
        if (rate.Packets < 1 || rate.Bytes < bytes || control && rate.Controls < 1)
        { SideChannelRateDrops++; return false; }
        rate.Packets--; rate.Bytes -= bytes;
        if (control) rate.Controls--;
        return true;
    }

    // A separate hook socket may bind only to a private peer on the same IP. Public NAT addresses
    // can represent unrelated players and therefore always require an exact endpoint.
    internal static bool MaySpeakFor(IPEndPoint peer, IPEndPoint from)
    {
        peer = StunCodec.Canonical(peer);
        from = StunCodec.Canonical(from);
        return peer.Equals(from) || IsPrivate(peer.Address) && peer.Address.Equals(from.Address);
    }

    private static bool IsPrivate(IPAddress address)
    {
        if (address.IsIPv4MappedToIPv6) address = address.MapToIPv4();
        if (IPAddress.IsLoopback(address)) return true;
        var bytes = address.GetAddressBytes();
        return address.AddressFamily == AddressFamily.InterNetwork
            ? bytes[0] == 10 || bytes[0] == 172 && bytes[1] is >= 16 and <= 31 || bytes[0] == 192 && bytes[1] == 168 || bytes[0] == 169 && bytes[1] == 254
            : (bytes[0] & 0xfe) == 0xfc || bytes[0] == 0xfe && (bytes[1] & 0xc0) == 0x80;
    }

    private void ReceiveSideChannel(ReadOnlySpan<byte> data, IPEndPoint endpoint)
    {
        switch (BinaryPrimitives.ReadUInt16BigEndian(data))
        {
            case CarStateCodec.Kind: ReceiveCarState(data, endpoint); break;
            case VoiceCodec.VoiceKind: ReceiveVoice(data, endpoint); break; // OD-147 (RelayEngine.Voice.cs)
            case VoiceCodec.ControlKind: ReceiveVoiceControl(data, endpoint); break;
        }
    }

    private void ReceiveCarState(ReadOnlySpan<byte> data, IPEndPoint endpoint)
    {
        if (CarStateCodec.ParseUp(data) is not { } up) return;
        var peer = _peers.FirstOrDefault(p => p.Endpoint.Equals(endpoint));
        if (peer is not null && peer.Xuid != up.Xuid) return;
        peer ??= _peers.FirstOrDefault(p => p.Synced && p.Xuid == up.Xuid && HookSource(p, endpoint));
        if (peer is null || !peer.Synced || !Allowed(peer.Xuid, peer.Endpoint) || !AllowedHook(peer.Xuid, endpoint)
            || !TakeSideChannelRate(peer, data.Length, true) || peer.CarStateAt is { } at && _now - at < 50) return;
        peer.CarStateAt = _now;
        peer.LastSeen = _now;
        peer.CarHook = endpoint;
        var changed = peer.CarState is not { } previous || previous.Lights != up.State.Lights || previous.Roof != up.State.Roof;
        peer.CarState = up.State;
        if (changed) PassCarState(peer);
    }

    private void PassCarState(RelayPeer from)
    {
        if (from.CarState is not { } state || !Allowed(from.Xuid, from.Endpoint)
            || from.CarHook is not { } source || !AllowedHook(from.Xuid, source)) return;
        var packet = CarStateCodec.WriteDown(new(state, from.Index, from.Xuid, from.CarPosition));
        foreach (var peer in _peers)
            if (peer != from && peer.Synced && Allowed(peer.Xuid, peer.Endpoint) && peer.CarHook is { } hook && AllowedHook(peer.Xuid, hook)) Emit(hook, packet);
    }

    private void TickSideChannels()
    {
        if (_now - _lastCarStates >= 1000)
        {
            _lastCarStates = _now;
            foreach (var peer in _peers) PassCarState(peer);
        }
        TickVoice();
    }

    private void OnPeerLeft(RelayPeer peer)
    {
        _sideChannelRates.Remove(peer);
        OnVoicePeerLeft(peer);
    }

    private void ClearSideChannels() { _sideChannelRates.Clear(); SideChannelRateDrops = 0; ClearVoice(); }
}
