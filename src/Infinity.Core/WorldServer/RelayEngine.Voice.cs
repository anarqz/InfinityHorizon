// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Numerics;

namespace Infinity.Core.WorldServer;

// OD-154 (VOICE_CHAT.md §3, §4, §9.1 as corrected by §10): the world relay is the voice authority. It never trusts a
// client for who speaks: a hook subscribes with its XUID (checked like car state), and its voice is then taken only
// from that endpoint and stamped with the sender's seat. Every mute layer is enforced here, before fan-out, so a
// client bug cannot leak a muted voice. Nothing is recorded: Opus packets pass through, only counters remain.
//
// CR-L46 (MATCHMAKING_API.md §5c "Proximity", OD-164) and OD-165 (rule 2a, team events): the host's voice mode (off, open,
// proximity) rides in every state down (0xC0F9). In proximity each (talker, listener) pair is decided here: same hearing group,
// same event side, within R = 150 m with 15 % hysteresis, at most the 4 nearest per listener, and the voice down carries both
// cars sampled at this instant (0xC0F8). In a team event each player hears only its hearing group (its team, or "outside the
// event"), in both open and proximity; state down is then written per listener, its talking bits only for what it hears.
internal sealed partial class RelayEngine
{
    internal const int VoicePacketsPerSecond = 20;
    internal const int VoiceBytesPerSecond = 2048;
    internal const int MaxTalkers = 4;
    internal const long TalkingMs = 300, VoiceStateMs = 1000;
    /// <summary>R (§5c, OD-164): the relay owns it and writes it in 0xC0F9; the host cannot choose it in v1.</summary>
    internal const ushort ProximityRange = 150;
    /// <summary>The in-range bit clears past 1.15 R, and a candidate replaces the farthest heard talker only when 1.15 × nearer.</summary>
    internal const double ProximityHysteresis = 1.15;
    /// <summary>A car whose last own prediction is older than this is unknown (§5c).</summary>
    internal const long CarKnownMs = 1500;
    /// <summary>Velocity's provisional scale, m/s per raw unit (±32767 = ±200 m/s; SAURON's live check pending).</summary>
    internal const double CarVelocityScale = 200.0 / 32767;
    private const double JumpSlackM = 10, JumpMaxIntervalS = 1.5;
    // CarPredictionFull (NerdCodec.CarBodyBits): position f32 x3 at 0, velocity s16 x3 at 96, orientation s16 x3 at 192
    private const int CarVelocityBit = 96, CarOrientationBit = 192;

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
        /// <summary>Proximity: H(L), the talkers this listener hears now (at most 4).</summary>
        internal HashSet<RelayPeer> Heard { get; } = [];
        /// <summary>Proximity: the talkers whose in-range bit with this listener is set.</summary>
        internal HashSet<RelayPeer> InRange { get; } = [];
    }

    private readonly record struct CarSample(bool Known, ushort Age, uint X, uint Y, uint Z, short Vx, short Vy, short Vz,
        short Qx, short Qy, short Qz, byte Jumps)
    {
        internal Vector3 Position => new(BitConverter.UInt32BitsToSingle(X), BitConverter.UInt32BitsToSingle(Y), BitConverter.UInt32BitsToSingle(Z));
        internal VoicePoseSide Side => Known ? new(true, Age, X, Y, Z, Vx, Vy, Vz, Jumps) : VoicePoseSide.None(Jumps);
    }

    private readonly Dictionary<RelayPeer, VoicePeer> _voice = [];
    // MATCHMAKING_API.md §5c: the seat byte of voice down and state down is a stable voice seat 0..11, one per synced player
    // (the lowest free when first needed, kept until the player leaves the world, then reused), never the peer index, which
    // grows to 254 as players come and go and would leave late joiners beyond the overlay's seats
    private readonly Dictionary<RelayPeer, byte> _voiceSeats = [];
    private readonly HashSet<ulong> _mutedForLobby = [];
    private readonly HashSet<RelayPeer> _voiceStateDue = [];
    private VoiceMode _voiceMode = VoiceMode.Open;
    private long _lastVoiceState;
    private bool _voiceChanged;

    /// <summary>Voice blocked for this XUID (a moderator ban or a community mute in its grant, CR-S67/S68); none before the server says.</summary>
    internal Func<ulong, bool>? VoiceBlocked { get; init; }

    /// <summary>Why voice was dropped, by reason (counters only: never audio). Proximity counts per (talker, listener).</summary>
    internal Dictionary<string, long> VoiceDrops { get; } = [];

    internal long VoiceForwarded { get; private set; }

    /// <summary>The lobby's voice is on (the mode is not off; state down's <c>lobby</c> bit).</summary>
    internal bool LobbyVoice => _voiceMode != VoiceMode.Off;

    /// <summary>The host's voice mode (§5c "The mode"; open by default).</summary>
    internal VoiceMode CurrentVoiceMode => _voiceMode;

    /// <summary>The pre-CR-L46 switch: on is <see cref="VoiceMode.Open"/>, off is <see cref="VoiceMode.Off"/>.</summary>
    internal void SetVoice(bool lobbyOn, IEnumerable<ulong> mutedForLobby) => SetVoice(lobbyOn ? VoiceMode.Open : VoiceMode.Off, mutedForLobby);

    /// <summary>
    /// The host's launcher: the voice mode and the players it muted for everyone. Takes effect at once: the next voice packet
    /// follows it, a mode change clears the proximity state (range bits, heard sets), and every subscriber gets a state down now.
    /// </summary>
    internal void SetVoice(VoiceMode mode, IEnumerable<ulong> mutedForLobby)
    {
        if (mode is not (VoiceMode.Off or VoiceMode.Open or VoiceMode.Proximity)) throw new ArgumentOutOfRangeException(nameof(mode));
        var muted = mutedForLobby.ToHashSet();
        if (_voiceMode == mode && _mutedForLobby.SetEquals(muted)) return;
        if (_voiceMode != mode)
            foreach (var v in _voice.Values) { v.Heard.Clear(); v.InRange.Clear(); }
        _voiceMode = mode;
        _mutedForLobby.Clear();
        _mutedForLobby.UnionWith(muted);
        SendVoiceStates();
    }

    /// <summary>The XUIDs heard talking now (the host's own overlay shows them; VoiceRoom.SetTalking).</summary>
    internal IReadOnlyList<ulong> Talking => _voice.Where(v => IsTalking(v.Value)).Select(v => v.Key.Xuid).ToList();

    /// <summary>Proximity: the XUIDs <paramref name="listener"/> hears now, H(L) (tests and diagnostics).</summary>
    internal IReadOnlyList<ulong> HeardBy(ulong listener) =>
        _voice.FirstOrDefault(v => v.Key.Xuid == listener).Value?.Heard.Select(p => p.Xuid).ToList() ?? [];

    private bool IsTalking(VoicePeer v) => v.LastVoiceAt is { } at && _now - at < TalkingMs && !v.SpurtEnded;

    private void DropVoice(string reason) => VoiceDrops[reason] = VoiceDrops.GetValueOrDefault(reason) + 1;

    /// <summary>The synced <paramref name="peer"/>'s voice seat (0..11); null only if all twelve are taken.</summary>
    private byte? VoiceSeatOf(RelayPeer peer)
    {
        if (_voiceSeats.TryGetValue(peer, out var seat)) return seat;
        for (byte s = 0; s < VoiceCodec.MaxSeats; s++)
            if (!_voiceSeats.ContainsValue(s)) { _voiceSeats[peer] = s; return s; }
        return null;
    }

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
            if (_voice.Remove(peer)) { _voiceChanged = true; ForgetHeard(peer); }
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
        // a muted or deafened listener stops hearing at once (a member leaves H(L) when it stops being a candidate)
        if (voice.Heard.RemoveWhere(t => (voice.Self & VoiceControlUp.Deafened) != 0 || voice.Mutes.Contains(t.Xuid)) != 0) _voiceStateDue.Add(peer);
        _voiceChanged = true;
        if (control.Op == VoiceControlUp.Subscribe) SendVoiceState(peer, voice); // a newcomer learns the lobby at once
        _voiceStateDue.Remove(peer);
        FlushVoiceStates();
    }

    private void ReceiveVoice(ReadOnlySpan<byte> data, IPEndPoint endpoint)
    {
        if (VoiceCodec.ParseUp(data) is not { } up) { DropVoice("malformed"); return; }
        var (peer, voice) = _voice.FirstOrDefault(v => v.Value.Hook.Equals(endpoint));
        if (peer is null || voice is null || !peer.Synced || !Allowed(peer.Xuid, peer.Endpoint) || !AllowedHook(peer.Xuid, endpoint)) { DropVoice("unbound"); return; }
        if (!TakeVoiceRate(voice, data.Length)) { DropVoice("rate"); return; }
        // the rules, in order (§5c; 2a, the hearing groups, is applied per listener below)
        if (_voiceMode == VoiceMode.Off) { DropVoice("lobby off"); return; }
        string? refused = VoiceBlocked?.Invoke(peer.Xuid) == true ? "blocked"
            : _mutedForLobby.Contains(peer.Xuid) ? "muted for lobby"
            : (voice.Self & VoiceControlUp.MicMuted) != 0 ? "mic muted" : null;
        if (refused is not null) { DropVoice(refused); ForgetHeard(peer); FlushVoiceStates(); return; }
        if (_voiceMode == VoiceMode.Proximity) { ReceiveProximity(peer, voice, up); return; }
        // rule 6: the cap of 4 talking counts the talkers of the sender's hearing group (the whole world outside team events)
        if (!IsTalking(voice) && _voice.Count(v => v.Key != peer && IsTalking(v.Value) && SameHearingGroup(v.Key, peer)) >= MaxTalkers)
        { DropVoice("talker cap"); return; }
        if (VoiceSeatOf(peer) is not { } seat) { DropVoice("no seat"); return; }
        StartPacket(voice, up);
        var packet = VoiceCodec.WriteDown(new VoiceDown(up.Header, seat, up.Opus));
        foreach (var (listener, heard) in _voice)
        {
            if (listener == peer || !Listening(listener, heard, peer)) continue;
            if (!SameHearingGroup(peer, listener)) { DropVoice("other group"); continue; }
            Emit(heard.Hook, packet);
            VoiceForwarded++;
        }
    }

    // rule 7's per-listener exclusions: subscribed, synced and allowed, not deafened, not muting the talker
    private bool Listening(RelayPeer listener, VoicePeer heard, RelayPeer talker) =>
        listener.Synced && Allowed(listener.Xuid, listener.Endpoint) && AllowedHook(listener.Xuid, heard.Hook)
        && (heard.Self & VoiceControlUp.Deafened) == 0 && !heard.Mutes.Contains(talker.Xuid);

    private void StartPacket(VoicePeer voice, VoiceUp up)
    {
        var started = !IsTalking(voice);
        voice.LastVoiceAt = _now;
        voice.SpurtEnded = (up.Header.Flags & VoiceHeader.EndOfSpurt) != 0;
        if (started || voice.SpurtEnded) _voiceChanged = true;
    }

    /// <summary>§5c "The relay's rules in proximity": per listener, side, range with hysteresis, the 4 nearest, then the pose.</summary>
    private void ReceiveProximity(RelayPeer peer, VoicePeer voice, VoiceUp up)
    {
        if (VoiceSeatOf(peer) is not { } seat) { DropVoice("no seat"); return; }
        StartPacket(voice, up);
        var talker = SampleCar(peer);
        foreach (var (listener, heard) in _voice)
        {
            if (listener == peer) continue;
            if (!Listening(listener, heard, peer)) { Unhear(listener, heard, peer); continue; }
            if (!SameHearingGroup(peer, listener)) { Unhear(listener, heard, peer); DropVoice("other group"); continue; }
            if (!SameSide(peer, listener)) { Unhear(listener, heard, peer); DropVoice("other side"); continue; }
            var me = SampleCar(listener);
            double? distance;
            if (talker.Known && me.Known)
            {
                var d = Vector3.Distance(talker.Position, me.Position);
                if (d <= ProximityRange) heard.InRange.Add(peer);
                else if (d > ProximityRange * ProximityHysteresis) heard.InRange.Remove(peer);
                if (!heard.InRange.Contains(peer)) { Unhear(listener, heard, peer); DropVoice("out of range"); continue; }
                distance = d;
            }
            else
            {
                heard.InRange.Remove(peer); // cleared while either car is unknown
                distance = me.Known ? ProximityRange : null; // a carless talker ranks last; a carless listener has no distances
            }
            if (!heard.Heard.Contains(peer))
            {
                if (heard.Heard.Count >= MaxTalkers)
                {
                    if (distance is not { } mine || Farthest(heard, me) is not { } far || !(mine * ProximityHysteresis < far.Distance))
                    { DropVoice("listener cap"); continue; }
                    heard.Heard.Remove(far.Peer);
                }
                heard.Heard.Add(peer);
                _voiceStateDue.Add(listener);
            }
            var pose = new VoicePose(talker.Side, me.Side, me.Known ? me.Qx : (short)0, me.Known ? me.Qy : (short)0, me.Known ? me.Qz : (short)0);
            Emit(heard.Hook, VoiceCodec.WriteDown(new VoiceDown(up.Header, seat, up.Opus, pose)));
            VoiceForwarded++;
        }
        if (voice.SpurtEnded) ForgetHeard(peer); // its spurt ended: it leaves every heard set
        FlushVoiceStates();
    }

    /// <summary>F: the farthest member of H(L) (a member whose car is unknown ranks R); among equally far, the higher seat.</summary>
    private (RelayPeer Peer, double Distance)? Farthest(VoicePeer heard, CarSample me)
    {
        (RelayPeer Peer, double Distance, int Seat)? far = null;
        foreach (var member in heard.Heard)
        {
            var sample = SampleCar(member);
            double d = sample.Known ? Vector3.Distance(sample.Position, me.Position) : ProximityRange;
            var seat = _voiceSeats.TryGetValue(member, out var s) ? s : -1;
            if (far is not { } f || d > f.Distance || d == f.Distance && seat > f.Seat) far = (member, d, seat);
        }
        return far is { } result ? (result.Peer, result.Distance) : null;
    }

    private void Unhear(RelayPeer listener, VoicePeer heard, RelayPeer talker)
    {
        if (heard.Heard.Remove(talker)) _voiceStateDue.Add(listener);
    }

    /// <summary>The talker leaves every heard set (its spurt ended, it left, or a rule now refuses it); with <paramref name="ranges"/>
    /// its range bits go too.</summary>
    private void ForgetHeard(RelayPeer talker, bool ranges = false)
    {
        foreach (var (listener, heard) in _voice)
        {
            Unhear(listener, heard, talker);
            if (ranges) heard.InRange.Remove(talker);
        }
    }

    /// <summary>The players' hearing groups changed (a team event began or ended, a player left it or got its team): heard sets
    /// keep only their own group, and everyone's state changes at once.</summary>
    private void VoiceGroupsChanged()
    {
        foreach (var (listener, heard) in _voice) heard.Heard.RemoveWhere(t => !SameHearingGroup(t, listener));
        SendVoiceStates();
    }

    /// <summary>
    /// A car prediction of the player's own car was accepted (RelayEngine.Packets.cs, before CarPosition and CarId move): the
    /// pose's receipt time, and the jump counter when the move is not explained by the last velocity (|Δp| &gt; |v_old| × Δt + 10 m,
    /// Δt capped at 1.5 s), the car handle changed, or the car is known again after being unknown.
    /// </summary>
    private void CarAccepted(RelayPeer peer, NerdMessage.Car car, Vector3 position)
    {
        if (peer.LastCarAt is { } at)
        {
            var jumped = _now - at > CarKnownMs || peer.CarId != car.Handle.Id;
            if (!jumped && peer.CarPosition is { } old && peer.LastCarBody is { } body)
            {
                var reader = new BitReader(body.Bytes, body.Length);
                reader.Skip(CarVelocityBit);
                var v = new Vector3((short)reader.Read(16), (short)reader.Read(16), (short)reader.Read(16)) * (float)CarVelocityScale;
                var dt = Math.Min(JumpMaxIntervalS, Math.Max(0, _now - at) / 1000.0);
                jumped = Vector3.Distance(position, old) > v.Length() * dt + JumpSlackM;
            }
            if (jumped) peer.CarJumps = unchecked((byte)(peer.CarJumps + 1));
        }
        peer.LastCarAt = _now;
        peer.LastCarBody = car.Body;
    }

    /// <summary>The peer's car at this instant: known when its last own prediction is at most 1500 ms old (RefreshCars never
    /// refreshes it). Position bits are copied as received, velocity and orientation raw.</summary>
    private CarSample SampleCar(RelayPeer peer)
    {
        if (peer.LastCarAt is not { } at || _now - at > CarKnownMs || peer.LastCarBody is not { } body || body.Length != NerdCodec.CarBodyBits)
            return new(false, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, peer.CarJumps);
        var reader = new BitReader(body.Bytes, body.Length);
        var (x, y, z) = ((uint)reader.Read(32), (uint)reader.Read(32), (uint)reader.Read(32));
        var (vx, vy, vz) = ((short)reader.Read(16), (short)reader.Read(16), (short)reader.Read(16));
        reader.Skip(CarOrientationBit - CarVelocityBit - 48);
        var (qx, qy, qz) = ((short)reader.Read(16), (short)reader.Read(16), (short)reader.Read(16));
        return new(true, (ushort)Math.Min(ushort.MaxValue, Math.Max(0, _now - at)), x, y, z, vx, vy, vz, qx, qy, qz, peer.CarJumps);
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

    /// <summary>State down is written per listener in proximity and during a team event (§5c "State down per listener").</summary>
    private bool VoiceStatePerListener => _voiceMode == VoiceMode.Proximity || _teamEvent;

    private VoiceStateDown VoiceStateFor(RelayPeer? listener)
    {
        var heard = listener is not null && _voice.TryGetValue(listener, out var l) ? l : null;
        var seats = new List<VoiceSeat>();
        foreach (var p in _peers.Where(p => p.Synced))
        {
            if (VoiceSeatOf(p) is not { } seat) continue;
            byte state = 0;
            var talking = _voice.TryGetValue(p, out var v) && IsTalking(v);
            // per listener: only the talkers it hears (its heard set in proximity, its hearing group in open); its own seat as is
            if (talking && listener is not null && p != listener)
                talking = _voiceMode == VoiceMode.Proximity ? heard?.Heard.Contains(p) == true : SameHearingGroup(p, listener);
            if (talking) state |= VoiceSeat.Talking;
            if (_mutedForLobby.Contains(p.Xuid)) state |= VoiceSeat.MutedForLobby;
            if (VoiceBlocked?.Invoke(p.Xuid) == true) state |= VoiceSeat.Blocked;
            seats.Add(new VoiceSeat(seat, p.Xuid, state));
        }
        var mode = new VoiceModeInfo(_voiceMode, _voiceMode == VoiceMode.Proximity ? ProximityRange : (ushort)0);
        return new(LobbyVoice ? VoiceStateDown.On : (byte)0, 3, seats, mode);
    }

    private void SendVoiceState(RelayPeer peer, VoicePeer voice)
    {
        if (peer.Synced && Allowed(peer.Xuid, peer.Endpoint) && AllowedHook(peer.Xuid, voice.Hook))
            Emit(voice.Hook, VoiceCodec.WriteState(VoiceStateFor(VoiceStatePerListener ? peer : null)));
    }

    /// <summary>Every subscriber's state, now.</summary>
    private void SendVoiceStates()
    {
        _voiceChanged = false;
        _voiceStateDue.Clear();
        _lastVoiceState = _now;
        foreach (var (peer, voice) in _voice) SendVoiceState(peer, voice);
    }

    /// <summary>A listener whose heard set changed gets its state at once (proximity, team events).</summary>
    private void FlushVoiceStates()
    {
        if (_voiceStateDue.Count == 0) return;
        foreach (var peer in _voiceStateDue.ToArray())
            if (_voice.TryGetValue(peer, out var voice)) SendVoiceState(peer, voice);
        _voiceStateDue.Clear();
    }

    private void TickVoice()
    {
        // a spurt that stopped without its last packet ends here, so "talking" never sticks
        foreach (var (peer, v) in _voice.ToArray())
            if (!v.SpurtEnded && v.LastVoiceAt is { } at && _now - at >= TalkingMs) { v.SpurtEnded = true; _voiceChanged = true; ForgetHeard(peer); }
        if (!_voiceChanged && _now - _lastVoiceState < VoiceStateMs) { FlushVoiceStates(); return; }
        SendVoiceStates();
    }

    private void OnVoicePeerLeft(RelayPeer peer)
    {
        _voiceChanged |= _voice.Remove(peer);
        _voiceChanged |= _voiceSeats.Remove(peer); // the seat is free for the next player
        ForgetHeard(peer, ranges: true);
    }

    private void ClearVoice()
    {
        _voice.Clear();
        _voiceSeats.Clear();
        _voiceStateDue.Clear();
        _mutedForLobby.Clear();
        _voiceMode = VoiceMode.Open;
        VoiceDrops.Clear();
        VoiceForwarded = 0;
    }
}
