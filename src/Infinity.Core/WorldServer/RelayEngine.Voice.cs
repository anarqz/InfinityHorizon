// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;

namespace Infinity.Core.WorldServer;

// OD-147 (VOICE_CHAT.md §3, §4, §9.1 as corrected by §10): the world relay is the voice authority. It never trusts a
// client for who speaks: a hook subscribes with its XUID (checked like car state), and its voice is then taken only
// from that endpoint and stamped with the sender's seat. Every mute layer is enforced here, before fan-out, so a
// client bug cannot leak a muted voice. Nothing is recorded: Opus packets pass through, only counters remain.
internal sealed partial class RelayEngine
{
    internal const int VoicePacketsPerSecond = 20;
    internal const int VoiceBytesPerSecond = 2048;
    internal const int MaxTalkers = 4;
    internal const long TalkingMs = 300, VoiceStateMs = 1000;

    private sealed class VoicePeer(IPEndPoint hook, long now)
    {
        internal IPEndPoint Hook { get; set; } = hook;
        internal byte Self { get; set; }
        internal HashSet<ulong> Mutes { get; } = [];
        internal long? LastVoiceAt { get; set; }
        internal bool SpurtEnded { get; set; } = true;
        internal long RateAt { get; set; } = now;
        internal double Packets { get; set; } = VoicePacketsPerSecond * 2.0;
        internal double Bytes { get; set; } = VoiceBytesPerSecond * 2.0;
    }

    private readonly Dictionary<RelayPeer, VoicePeer> _voice = [];
    private readonly HashSet<ulong> _mutedForLobby = [];
    private bool _lobbyVoice = true;
    private long _lastVoiceState;
    private bool _voiceChanged;

    /// <summary>Voice blocked for this XUID (a moderator ban or a community mute in its grant, CR-S67/S68); none before the server says.</summary>
    internal Func<ulong, bool>? VoiceBlocked { get; init; }

    /// <summary>Why voice was dropped, by reason (counters only: never audio).</summary>
    internal Dictionary<string, long> VoiceDrops { get; } = [];

    internal long VoiceForwarded { get; private set; }

    /// <summary>The lobby's voice chat switch (the host's, OD-147).</summary>
    internal bool LobbyVoice => _lobbyVoice;

    /// <summary>The host's launcher: the lobby's switch and the players it muted for everyone. Takes effect at once.</summary>
    internal void SetVoice(bool lobbyOn, IEnumerable<ulong> mutedForLobby)
    {
        var muted = mutedForLobby.ToHashSet();
        if (_lobbyVoice == lobbyOn && _mutedForLobby.SetEquals(muted)) return;
        _lobbyVoice = lobbyOn;
        _mutedForLobby.Clear();
        _mutedForLobby.UnionWith(muted);
        _voiceChanged = true;
    }

    /// <summary>The XUIDs heard talking now (the host's own overlay shows them; VoiceRoom.SetTalking).</summary>
    internal IReadOnlyList<ulong> Talking => _voice.Where(v => IsTalking(v.Value)).Select(v => v.Key.Xuid).ToList();

    private bool IsTalking(VoicePeer v) => v.LastVoiceAt is { } at && _now - at < TalkingMs && !v.SpurtEnded;

    private void DropVoice(string reason) => VoiceDrops[reason] = VoiceDrops.GetValueOrDefault(reason) + 1;

    private void ReceiveVoiceControl(ReadOnlySpan<byte> data, IPEndPoint endpoint)
    {
        if (VoiceCodec.ParseControl(data) is not { } control) { DropVoice("malformed"); return; }
        var peer = _peers.FirstOrDefault(p => p.Endpoint.Equals(endpoint));
        if (peer is not null && peer.Xuid != control.Xuid) { DropVoice("spoofed"); return; }
        peer ??= _peers.FirstOrDefault(p => p.Synced && p.Xuid == control.Xuid && HookSource(p, endpoint));
        if (peer is null || !peer.Synced || !Allowed(peer.Xuid, peer.Endpoint) || !AllowedHook(peer.Xuid, endpoint)
            || !TakeSideChannelRate(peer, data.Length, true)) { DropVoice("unauthorized"); return; }
        peer.LastSeen = _now;
        if (control.Op == VoiceControlUp.Leave)
        {
            _voiceChanged |= _voice.Remove(peer);
            return;
        }
        if (!_voice.TryGetValue(peer, out var voice))
        {
            if (control.Op != VoiceControlUp.Subscribe) { DropVoice("not subscribed"); return; }
            _voice[peer] = voice = new VoicePeer(endpoint, _now);
        }
        voice.Hook = endpoint;
        voice.Self = control.Self;
        voice.Mutes.Clear();
        voice.Mutes.UnionWith(control.Mutes);
        _voiceChanged = true;
        if (control.Op == VoiceControlUp.Subscribe) SendVoiceState(peer, voice); // a newcomer learns the lobby at once
    }

    private void ReceiveVoice(ReadOnlySpan<byte> data, IPEndPoint endpoint)
    {
        if (VoiceCodec.ParseUp(data) is not { } up) { DropVoice("malformed"); return; }
        var (peer, voice) = _voice.FirstOrDefault(v => v.Value.Hook.Equals(endpoint));
        if (peer is null || voice is null || !peer.Synced || !Allowed(peer.Xuid, peer.Endpoint) || !AllowedHook(peer.Xuid, endpoint)) { DropVoice("unbound"); return; }
        if (!TakeVoiceRate(voice, data.Length)) { DropVoice("rate"); return; }
        // the rules, in order (§9.1)
        if (!_lobbyVoice) { DropVoice("lobby off"); return; }
        if (VoiceBlocked?.Invoke(peer.Xuid) == true) { DropVoice("blocked"); return; }
        if (_mutedForLobby.Contains(peer.Xuid)) { DropVoice("muted for lobby"); return; }
        if ((voice.Self & VoiceControlUp.MicMuted) != 0) { DropVoice("mic muted"); return; }
        if (!IsTalking(voice) && _voice.Values.Count(IsTalking) >= MaxTalkers) { DropVoice("talker cap"); return; }
        var started = !IsTalking(voice);
        voice.LastVoiceAt = _now;
        voice.SpurtEnded = (up.Header.Flags & VoiceHeader.EndOfSpurt) != 0;
        if (started || voice.SpurtEnded) _voiceChanged = true;
        var packet = VoiceCodec.WriteDown(new VoiceDown(up.Header, peer.Index, up.Opus));
        foreach (var (listener, heard) in _voice)
        {
            if (listener == peer || !listener.Synced || !Allowed(listener.Xuid, listener.Endpoint) || !AllowedHook(listener.Xuid, heard.Hook)) continue;
            if ((heard.Self & VoiceControlUp.Deafened) != 0 || heard.Mutes.Contains(peer.Xuid)) continue;
            Emit(heard.Hook, packet);
            VoiceForwarded++;
        }
    }

    private bool TakeVoiceRate(VoicePeer v, int bytes)
    {
        var elapsed = Math.Max(0, _now - v.RateAt) / 1000.0; v.RateAt = _now;
        v.Packets = Math.Min(VoicePacketsPerSecond * 2.0, v.Packets + elapsed * VoicePacketsPerSecond);
        v.Bytes = Math.Min(VoiceBytesPerSecond * 2.0, v.Bytes + elapsed * VoiceBytesPerSecond);
        if (v.Packets < 1 || v.Bytes < bytes) return false;
        v.Packets--; v.Bytes -= bytes;
        return true;
    }

    private VoiceStateDown VoiceStateFor()
    {
        var seats = _peers.Where(p => p.Synced).Take(VoiceCodec.MaxSeats).Select(p =>
        {
            byte state = 0;
            if (_voice.TryGetValue(p, out var v) && IsTalking(v)) state |= VoiceSeat.Talking;
            if (_mutedForLobby.Contains(p.Xuid)) state |= VoiceSeat.MutedForLobby;
            if (VoiceBlocked?.Invoke(p.Xuid) == true) state |= VoiceSeat.Blocked;
            return new VoiceSeat(p.Index, p.Xuid, state);
        }).ToList();
        return new(_lobbyVoice ? VoiceStateDown.On : (byte)0, 3, seats);
    }

    private void SendVoiceState(RelayPeer peer, VoicePeer voice)
    {
        if (peer.Synced && Allowed(peer.Xuid, peer.Endpoint) && AllowedHook(peer.Xuid, voice.Hook)) Emit(voice.Hook, VoiceCodec.WriteState(VoiceStateFor()));
    }

    private void TickVoice()
    {
        // a spurt that stopped without its last packet ends here, so "talking" never sticks
        foreach (var v in _voice.Values)
            if (!v.SpurtEnded && v.LastVoiceAt is { } at && _now - at >= TalkingMs) { v.SpurtEnded = true; _voiceChanged = true; }
        if (!_voiceChanged && _now - _lastVoiceState < VoiceStateMs) return;
        _voiceChanged = false;
        _lastVoiceState = _now;
        foreach (var (peer, voice) in _voice) SendVoiceState(peer, voice);
    }

    private void OnVoicePeerLeft(RelayPeer peer) => _voiceChanged |= _voice.Remove(peer);

    private void ClearVoice()
    {
        _voice.Clear();
        _mutedForLobby.Clear();
        _lobbyVoice = true;
        VoiceDrops.Clear();
        VoiceForwarded = 0;
    }
}
