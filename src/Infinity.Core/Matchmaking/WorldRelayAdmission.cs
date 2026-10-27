// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Net;
using System.Net.Sockets;
using System.Security.Cryptography;
using Infinity.Core.WorldServer;

namespace Infinity.Core.Matchmaking;

/// <summary>Authenticated control-plane snapshot. UDP identity alone never opens a seat.</summary>
/// <remarks>
/// <paramref name="devLanPeers"/> is the development stack's stand-in for the missing peer
/// endpoint proof (WORLD_SERVER_PORT.md, platform request 1): a peer with a valid signed grant
/// for this world and host is admitted from any RFC 1918 address, the endpoint unchecked. The
/// shell sets it only for a private/loopback plain-http server and matchmaking; never in a release.
/// </remarks>
internal sealed class WorldRelayAdmission(ulong ownXuid, RelayGrantVerifyKeys keys, TimeProvider? clock = null, bool devLanPeers = false, bool requireSocketProof = false)
{
    private sealed record Entry(string GrantId, long ExpiresAt, TimeSpan Lifetime, long StartedAt);
    private sealed record ProofGrant(ulong Peer, Entry Lease, UdpProofHost.Authority Authority);
    private sealed record Snapshot(long StartedAt, DateTimeOffset HostUntil, Dictionary<(ulong Xuid, IPEndPoint Endpoint), Entry> Entries,
        Dictionary<ulong, Entry> LanEntries, Dictionary<Guid,ProofGrant> ProofGrants);
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly DateTimeOffset _originUtc = (clock ?? TimeProvider.System).GetUtcNow();
    private readonly long _originTimestamp = (clock ?? TimeProvider.System).GetTimestamp();
    private long _lastUtcTicks;
    private readonly object _gate = new();
    private readonly Dictionary<string, long> _revoked = new(StringComparer.Ordinal);
    private Snapshot? _snapshot;
    private IReadOnlyList<RelayLegSpec> _legs = [];
    private string? _worldId;
    private WorldAnnounced? _world;
    private UdpProofHost? _proof;
    private UdpProofHost Proof => _proof ??= new(ProofAuthority,_clock,()=>EffectiveNow().ToUnixTimeSeconds());
    internal string DiagnosticSummary() {lock(_gate)return Proof.DiagnosticSummary();}
    private bool RequireProof => BuildProfile.ProductionOnly || requireSocketProof;
    private static readonly TimeSpan HostLease = TimeSpan.FromSeconds(45);

    internal void Update(WorldAnnounced world)
    {
        try { UpdateCore(world, true); }
        catch { Clear(); throw; }
    }

    internal void Refresh(Punches punches)
    {
        ArgumentNullException.ThrowIfNull(punches);
        try
        {
            lock (_gate)
                UpdateCore((_world ?? throw new InvalidOperationException("No integrated world context")) with { Punches = punches.Items }, false);
        }
        catch { Clear(); throw; }
    }

    private void UpdateCore(WorldAnnounced world, bool renewHostLease)
    {
        ArgumentNullException.ThrowIfNull(world);
        lock (_gate)
        {
            // Keep the old verified snapshot usable while its replacement is built. Publishing
            // an intermediate empty policy would disconnect peers on every HTTP refresh.
            var previousSnapshot = Volatile.Read(ref _snapshot);
            if (ownXuid == 0 || keys.Count == 0 || world.Host is null || world.Punches is null || world.Punches.Any(p => p is null)
                || world.Host.Xuid != ownXuid.ToString(CultureInfo.InvariantCulture)
                || world.Game != "fh2" || !Guid.TryParseExact(world.Id, "D", out var id) || id == Guid.Empty
                || world.Id != id.ToString("D") || _worldId is { } previous && previous != world.Id
                || world.Punches.Count > 4096) throw new InvalidOperationException("Invalid integrated world context");
            _worldId = world.Id;
            var now = EffectiveNow();
            var started = _clock.GetTimestamp();
            foreach (var revoked in _revoked.Where(p => _clock.GetElapsedTime(p.Value, started) >= TimeSpan.FromSeconds(RelayGrantVerifier.MaxLifetimeSeconds + RelayGrantVerifier.FutureSkewSeconds)).Select(p => p.Key).ToArray())
                _revoked.Remove(revoked);
            foreach (var kicked in world.Punches.Where(p => p.Kicked))
                if (_revoked.Count < 4096 && Guid.TryParseExact(kicked.GrantId, "D", out _)) _revoked[kicked.GrantId] = started;
            var entries = new Dictionary<(ulong, IPEndPoint), Entry>();
            var lanEntries = new Dictionary<ulong, Entry>();
            var proofGrants = new Dictionary<Guid,ProofGrant>();
            var legs = new List<RelayLegSpec>();
            var duplicateGrants = world.Punches.GroupBy(p => p.GrantId, StringComparer.Ordinal).Where(p => p.Count() > 1).Select(p => p.Key).ToHashSet(StringComparer.Ordinal);
            foreach (var punch in world.Punches)
            {
                if (punch.Relay is null || punch.Kicked || duplicateGrants.Contains(punch.GrantId) || _revoked.ContainsKey(punch.GrantId) || _revoked.Count >= 4096
                    || !ulong.TryParse(punch.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid) || xuid == ownXuid) continue;
                IPAddress? address = null;
                var observed = punch.Endpoint is { Port: > 0 } && IPAddress.TryParse(punch.Endpoint.Ip, out address) && Unicast(address);
                var proofKey = punch.Relay.UdpProof?.Decode();
                if (!observed && !devLanPeers && proofKey is null) continue;
                RelayGrantClaims grant;
                try
                {
                    grant = RelayGrantVerifier.Verify(punch.Relay, keys, now.ToUnixTimeSeconds(),
                        new(world.Id, world.Host.Xuid, punch.Xuid, "host", punch.GrantId));
                }
                catch (RelayGrantException) { if(proofKey is not null) CryptographicOperations.ZeroMemory(proofKey); continue; }
                // The endpoint must come from the authenticated platform observation, never an
                // OpenConnection XUID or a same-IP guess. Missing observations fail closed.
                var lifetime = DateTimeOffset.FromUnixTimeSeconds(grant.Exp) - now;
                var entry = new Entry(grant.Grant, grant.Exp, lifetime < HostLease ? lifetime : HostLease, started);
                if(proofKey is not null)
                    proofGrants[Guid.Parse(grant.Grant)] = new(xuid,entry,new(UdpProofCodec.Header(Guid.Parse(grant.Grant),id,ownXuid,xuid),proofKey,grant.Exp));
                if (observed) entries[(xuid, StunCodec.Canonical(new(address!, punch.Endpoint!.Port)))] = entry;
                // CR-S57: the verified grant names the relay both sides meet at; only that signed relay is ever contacted
                if (grant.Relay is { } relay && IPAddress.TryParse(relay.Addr, out var relayAddress) && relayAddress.AddressFamily == AddressFamily.InterNetwork)
                    legs.Add(new(Guid.Parse(grant.Grant), Convert.FromBase64String(punch.Relay.Claims), Convert.FromBase64String(punch.Relay.Signature),
                        StunCodec.Canonical(new IPEndPoint(relayAddress, relay.Port)), punch.Transport == Transports.Relay || world.Transport == Transports.Relay));
                if (devLanPeers) lanEntries[xuid] = entry;
            }
            _world = world;
            _legs = legs.AsReadOnly();
            Volatile.Write(ref _snapshot, new(renewHostLease ? started : previousSnapshot?.StartedAt ?? started,
                renewHostLease ? now + HostLease : previousSnapshot?.HostUntil ?? now, entries, lanEntries,proofGrants));
            _proof?.Reconcile();
            if(previousSnapshot is not null) foreach(var old in previousSnapshot.ProofGrants.Values) CryptographicOperations.ZeroMemory(old.Authority.Key);
        }
    }

    /// <summary>CR-S57: the grants whose punch names a relay (IPv4 literals; a DNS name waits for a later build), from the last snapshot.</summary>
    internal IReadOnlyList<RelayLegSpec> RelayLegs() { lock (_gate) return _legs; }

    internal bool Allowed(ulong xuid, IPEndPoint endpoint, long unused)
        => AllowedKind(xuid,endpoint,0);
    internal bool AllowedHook(ulong xuid,IPEndPoint endpoint,long unused) => AllowedKind(xuid,endpoint,1);
    private bool HostLive(Snapshot snapshot,DateTimeOffset now) => _clock.GetElapsedTime(snapshot.StartedAt) is var elapsed
        && elapsed>=TimeSpan.Zero && elapsed<HostLease && now<snapshot.HostUntil;
    private UdpProofHost.Authority? ProofAuthority(Guid grant)
    {
        var snapshot = Volatile.Read(ref _snapshot);
        if(snapshot is null || !HostLive(snapshot,EffectiveNow()) || !snapshot.ProofGrants.TryGetValue(grant,out var proof)
            || !Live(proof.Lease,EffectiveNow()))return null;
        return proof.Authority;
    }
    internal byte[]? ReceiveProof(ReadOnlySpan<byte> data,IPEndPoint endpoint)
    { lock(_gate) return Proof.Receive(data,endpoint); }
    internal IReadOnlyList<(Guid Grant,uint Revision,IPEndPoint[] Endpoints)> NetworkHints()
    {
        lock(_gate)
        {
            var result=new List<(Guid,uint,IPEndPoint[])>();var now=EffectiveNow();
            foreach(var punch in _world?.Punches ?? [])
            {
                if(result.Count>=64)break;
                if(punch.Kicked || punch.Candidates is not {Revision:>0} hints || !Guid.TryParse(punch.GrantId,out var grant)
                    || ProofAuthority(grant) is null || !DateTimeOffset.TryParse(hints.GeneratedAt,CultureInfo.InvariantCulture,System.Globalization.DateTimeStyles.AssumeUniversal,out var generated)
                    || !DateTimeOffset.TryParse(hints.ExpiresAt,CultureInfo.InvariantCulture,System.Globalization.DateTimeStyles.AssumeUniversal,out var expires)
                    || expires<=now || expires-generated>TimeSpan.FromSeconds(30) || generated>now+TimeSpan.FromSeconds(1))continue;
                try {result.Add((grant,hints.Revision,RelayNetworkFrames.Endpoints(hints.Candidates)));}
                catch(ProtocolException) { }
            }
            return result;
        }
    }
    private bool AllowedKind(ulong xuid,IPEndPoint endpoint,byte kind)
    {
        // Own-host loopback uses only the immutable lease snapshot, so a refresh can build
        // its replacement without blocking local game traffic behind signature verification.
        if(xuid==ownXuid)
        {
            var local=Volatile.Read(ref _snapshot);
            return local is not null && HostLive(local,EffectiveNow()) && IPAddress.IsLoopback(StunCodec.Canonical(endpoint).Address);
        }
        lock(_gate)
        {
        var snapshot = Volatile.Read(ref _snapshot);
        if(snapshot is null)return false;
        var now = EffectiveNow();
        if(!HostLive(snapshot,now)) return false;
        endpoint = StunCodec.Canonical(endpoint);
        if (xuid == ownXuid) return IPAddress.IsLoopback(endpoint.Address);
        if(Proof.Allowed(xuid,kind,endpoint))return true;
        if(RequireProof)return false;
        if (snapshot.Entries.TryGetValue((xuid, endpoint), out var entry) && Live(entry, now)) return true;
        var address = endpoint.Address.IsIPv4MappedToIPv6 ? endpoint.Address.MapToIPv4() : endpoint.Address;
        return devLanPeers && HostRelay.IsPrivate(address) && snapshot.LanEntries.TryGetValue(xuid, out var lan) && Live(lan, now);
        }
    }

    private bool Live(Entry entry, DateTimeOffset now) =>
        now.ToUnixTimeSeconds() < entry.ExpiresAt && _clock.GetElapsedTime(entry.StartedAt) < entry.Lifetime;

    internal void Revoke(ulong xuid)
    {
        lock (_gate)
        {
            var snapshot = Volatile.Read(ref _snapshot);
            if (snapshot is null) return;
            var next = new Dictionary<(ulong Xuid, IPEndPoint Endpoint), Entry>(snapshot.Entries);
            foreach (var pair in snapshot.Entries.Where(p => p.Key.Xuid == xuid))
            {
                if (_revoked.Count < 4096) _revoked[pair.Value.GrantId] = _clock.GetTimestamp();
                next.Remove(pair.Key);
            }
            var lan = new Dictionary<ulong, Entry>(snapshot.LanEntries);
            if (lan.Remove(xuid, out var removed) && _revoked.Count < 4096) _revoked[removed.GrantId] = _clock.GetTimestamp();
            // A peer may hold several live grants (a rejoin before the old punch is gone); the
            // stored entry keeps one, so every grant of this XUID in the snapshot is revoked.
            var id = xuid.ToString(CultureInfo.InvariantCulture);
            foreach (var punch in _world?.Punches ?? [])
                if (punch.Xuid == id && _revoked.Count < 4096 && Guid.TryParseExact(punch.GrantId, "D", out _))
                    _revoked[punch.GrantId] = _clock.GetTimestamp();
            var proof=new Dictionary<Guid,ProofGrant>(snapshot.ProofGrants);
            foreach(var item in proof.Where(p=>p.Value.Peer==xuid).ToArray()) { CryptographicOperations.ZeroMemory(item.Value.Authority.Key);proof.Remove(item.Key); }
            Volatile.Write(ref _snapshot, snapshot with { Entries = next, LanEntries = lan,ProofGrants=proof });
            _proof?.Reconcile();
        }
    }

    internal void Clear() { lock (_gate) { _world = null; _legs = []; if(_snapshot is { } s) foreach(var p in s.ProofGrants.Values)CryptographicOperations.ZeroMemory(p.Authority.Key); Volatile.Write(ref _snapshot, null); _proof?.Clear(); } }

    private DateTimeOffset EffectiveNow()
    {
        var elapsed = _clock.GetElapsedTime(_originTimestamp);
        var monotonicUtc = _originUtc + (elapsed < TimeSpan.Zero ? TimeSpan.Zero : elapsed);
        var wallUtc = _clock.GetUtcNow();
        var candidate = (wallUtc > monotonicUtc ? wallUtc : monotonicUtc).UtcTicks;
        var observed = Volatile.Read(ref _lastUtcTicks);
        while (true)
        {
            var next = Math.Max(candidate, observed);
            var previous = Interlocked.CompareExchange(ref _lastUtcTicks, next, observed);
            if (previous == observed) return new(next, TimeSpan.Zero);
            observed = previous;
        }
    }

    private static bool Unicast(IPAddress address)
    {
        if (address.IsIPv4MappedToIPv6) address = address.MapToIPv4();
        return address.AddressFamily == AddressFamily.InterNetwork
            ? address.GetAddressBytes()[0] is > 0 and < 224
            : !address.Equals(IPAddress.IPv6Any) && !address.IsIPv6Multicast;
    }
}
