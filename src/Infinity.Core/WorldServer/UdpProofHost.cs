// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Security.Cryptography;

namespace Infinity.Core.WorldServer;

/// <summary>Bounded FIUP state, called under the world's admission lock; lookup is authenticated HTTPS authority.</summary>
internal sealed class UdpProofHost(Func<Guid, UdpProofHost.Authority?> lookup, TimeProvider clock, Func<long> unixNow)
{
    internal sealed record Authority(byte[] Header, byte[] Key, long Expires)
    { public override string ToString() => "FIUP authority { redacted }"; }
    private sealed record Pending(UdpProofCodec.Packet Hello, IPEndPoint Endpoint, byte[] Challenge, long At, bool Renewal,byte[] Fingerprint);
    private sealed record Completed(UdpProofCodec.Packet Response, IPEndPoint Endpoint, byte[] Ack, long At,byte[] Fingerprint);
    private sealed record Binding(Guid Grant, ulong Peer, byte Kind, IPEndPoint Endpoint, byte[] KeyHash, long At);
    private sealed record Seen(Guid Grant, byte Kind, byte[] Nonce,IPEndPoint Endpoint, long At,byte[] Fingerprint);
    private sealed class PreRate(long at) { internal long At=at; internal int Count; internal Dictionary<IPAddress,int> Ips=[]; }
    private readonly List<Pending> _pending=[];
    private readonly List<Completed> _completed=[];
    private readonly List<Binding> _bindings=[];
    private readonly List<Seen> _seen=[];
    private readonly Dictionary<Guid,(long At,int Count,int New)> _rates=[];
    private readonly Dictionary<Guid,PreRate> _pre=[];
    private long _rateAt;
    private long _preAt;
    private int _newCount,_renewCount,_retryCount;
    private readonly uint[] _diagnostic=new uint[9];
    private void Count(int slot) {if(_diagnostic[slot]!=uint.MaxValue)_diagnostic[slot]++;}
    internal string DiagnosticSummary()=>FormattableString.Invariant($"udp host hello_game={_diagnostic[0]} hello_hook={_diagnostic[1]} response_game={_diagnostic[2]} response_hook={_diagnostic[3]} challenge_game={_diagnostic[4]} challenge_hook={_diagnostic[5]} ack_game={_diagnostic[6]} ack_hook={_diagnostic[7]} rejected={_diagnostic[8]}");

    private bool Fresh(long at, int seconds) => clock.GetElapsedTime(at) is var elapsed && elapsed>=TimeSpan.Zero && elapsed<TimeSpan.FromSeconds(seconds);
    private bool Matches(Authority? authority, UdpProofCodec.Packet p) => authority is not null && authority.Expires>unixNow()
        && UdpProofCodec.SameContext(p,authority.Header);
    private static byte[] Fingerprint(Authority authority)=>SHA256.HashData([..authority.Header,..authority.Key]);
    private bool SameAuthority(Guid grant,byte[] fingerprint)=>lookup(grant) is { } authority
        && authority.Expires>unixNow() && CryptographicOperations.FixedTimeEquals(Fingerprint(authority),fingerprint);
    private bool Live(Binding binding)
    {
        var authority=lookup(binding.Grant);
        return authority is not null && authority.Expires>unixNow() && Fresh(binding.At,45)
            && CryptographicOperations.FixedTimeEquals(Fingerprint(authority),binding.KeyHash);
    }
    private void Prune()
    {
        _bindings.RemoveAll(b=>!Live(b));
        _pending.RemoveAll(p=>!Fresh(p.At,5) || UdpProofCodec.Parse(p.Challenge)!.Expires <= (ulong)Math.Max(0,unixNow()) || !SameAuthority(p.Hello.Grant,p.Fingerprint));
        _completed.RemoveAll(p=>!Fresh(p.At,5) || p.Response.Expires<=(ulong)Math.Max(0,unixNow()) || !SameAuthority(p.Response.Grant,p.Fingerprint));
        _seen.RemoveAll(p=>!Fresh(p.At,90) || !SameAuthority(p.Grant,p.Fingerprint));
        foreach(var grant in _rates.Where(r=>!Fresh(r.Value.At,1)).Select(r=>r.Key).ToArray()) _rates.Remove(grant);
        foreach(var grant in _pre.Where(r=>!Fresh(r.Value.At,1)).Select(r=>r.Key).ToArray())_pre.Remove(grant);
        if (!Fresh(_rateAt,1)) { _rateAt=clock.GetTimestamp(); _newCount=_renewCount=_retryCount=0; }
    }
    internal void Reconcile()=>Prune();
    private bool Preliminary(Guid grant,IPAddress ip)
    {
        if(!Fresh(_preAt,1)) { _pre.Clear();_preAt=clock.GetTimestamp(); }
        PreRate? rate=_pre.GetValueOrDefault(grant);
        if(rate is not null && !Fresh(rate.At,1)) { _pre.Remove(grant);rate=null; }
        if(rate is null) { if(_pre.Count>=4096)return false;_pre[grant]=rate=new(clock.GetTimestamp()); }
        if(rate.Count>=64 || rate.Ips.GetValueOrDefault(ip)>=16 || rate.Ips.Count>=16 && !rate.Ips.ContainsKey(ip))return false;
        rate.Count++;rate.Ips[ip]=rate.Ips.GetValueOrDefault(ip)+1;return true;
    }
    internal bool Allowed(ulong peer, byte kind, IPEndPoint endpoint)
    {
        Prune(); endpoint=StunCodec.Canonical(endpoint);
        return _bindings.Any(b=>b.Peer==peer && b.Kind==kind && b.Endpoint.Equals(endpoint));
    }
    internal byte[]? Receive(ReadOnlySpan<byte> data, IPEndPoint source)
    {
        var packet=UdpProofCodec.Parse(data);
        if(packet is {Type:1 or 3})Count((packet.Type==1?0:2)+packet.Kind);
        var answer=ReceiveCore(data,source);
        if(answer is {Length:147})Count((answer[5]==2?4:6)+answer[6]);
        else Count(8);
        return answer;
    }
    private byte[]? ReceiveCore(ReadOnlySpan<byte> data, IPEndPoint source)
    {
        var p=UdpProofCodec.Parse(data); if(p is null || p.Type is not (1 or 3)) return null;
        source=StunCodec.Canonical(source); if(source.Port==0 || !UdpProofCodec.Unicast(source.Address))return null;
        var authority=lookup(p.Grant);
        if(!Matches(authority,p) || !Preliminary(p.Grant,source.Address) || !UdpProofCodec.Verify(p,authority!.Key)) return null;
        Prune();
        if(p.Type==1)
        {
            var cached=_pending.FirstOrDefault(x=>x.Endpoint.Equals(source) && x.Hello.Raw.AsSpan().SequenceEqual(p.Raw));
            if(cached is not null) return _retryCount++<16 ? cached.Challenge.ToArray() : null;
            // A consumed nonce cannot create a new lifetime, including after its pending record was consumed.
            if(_completed.Any(x=>x.Response.Grant==p.Grant && x.Response.Kind==p.Kind && x.Response.Raw.AsSpan(56,16).SequenceEqual(p.Raw.AsSpan(56,16)))) return null;
            if(_seen.Any(x=>x.Grant==p.Grant && x.Kind==p.Kind && x.Endpoint.Equals(source) && x.Nonce.AsSpan().SequenceEqual(p.Raw.AsSpan(56,16))) || _seen.Count>=4096) return null;
            var renewal=_bindings.Any(b=>b.Grant==p.Grant && b.Endpoint.Equals(source));
            if(_bindings.Any(b=>b.Peer!=p.Peer && b.Endpoint.Equals(source))) return null;
            var rate=_rates.GetValueOrDefault(p.Grant);
            var established=_bindings.Any(b=>b.Grant==p.Grant);
            if(rate.Count>=2 || !renewal && established && rate.New>=1 || _rates.Count>=4096 && !_rates.ContainsKey(p.Grant))return null;
            if(renewal ? _renewCount>=32 : _newCount>=32) return null;
            if(_pending.Count>=64 || !renewal && _pending.Count(x=>!x.Renewal)>=32
                || _pending.Count(x=>x.Hello.Grant==p.Grant && x.Hello.Kind==p.Kind)>=2
                || !renewal && established && _pending.Any(x=>x.Hello.Grant==p.Grant && x.Hello.Kind==p.Kind && !x.Renewal)) return null;
            _rates[p.Grant]=(rate.Count==0 ? clock.GetTimestamp():rate.At,rate.Count+1,rate.New+(renewal?0:1));
            if(renewal)_renewCount++;else _newCount++;
            var expiry=(ulong)Math.Min(unixNow()+5,authority!.Expires);
            var challenge=UdpProofCodec.Challenge(p,source,expiry,authority.Key);
            var fingerprint=Fingerprint(authority);
            _seen.Add(new(p.Grant,p.Kind,p.Raw.AsSpan(56,16).ToArray(),source,clock.GetTimestamp(),fingerprint));
            _pending.Add(new(p,source,challenge,clock.GetTimestamp(),renewal,fingerprint)); return challenge.ToArray();
        }
        var completed=_completed.FirstOrDefault(x=>x.Endpoint.Equals(source) && x.Response.Raw.AsSpan().SequenceEqual(p.Raw));
        if(completed is not null) return _retryCount++<16 ? completed.Ack.ToArray() : null;
        var pending=_pending.FirstOrDefault(x=>x.Endpoint.Equals(source) && x.Hello.Grant==p.Grant && x.Hello.Kind==p.Kind
            && x.Challenge.AsSpan(8,107).SequenceEqual(p.Raw.AsSpan(8,107)));
        if(pending is null || !p.Observed!.Equals(source) || p.Expires<=(ulong)Math.Max(0,unixNow())
            || _bindings.Any(b=>b.Peer!=p.Peer && b.Endpoint.Equals(source))) return null;
        if(_bindings.Count>=64 && !_bindings.Any(b=>b.Grant==p.Grant && b.Kind==p.Kind))return null;
        _pending.Remove(pending);
        _bindings.RemoveAll(b=>b.Grant==p.Grant && b.Kind==p.Kind);
        _bindings.Add(new(p.Grant,p.Peer,p.Kind,source,Fingerprint(authority!),clock.GetTimestamp()));
        var ack=UdpProofCodec.ChangeType(data,4,authority.Key);
        if(_completed.Count==64)_completed.RemoveAt(0);
        _completed.Add(new(p,source,ack,clock.GetTimestamp(),Fingerprint(authority!))); return ack;
    }
    internal void Clear() { _pending.Clear();_completed.Clear();_bindings.Clear();_seen.Clear();_rates.Clear();_pre.Clear(); }
}
