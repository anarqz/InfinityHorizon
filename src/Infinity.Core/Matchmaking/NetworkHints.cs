// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Net;
using System.Net.Sockets;
using Infinity.Core.Wire;

namespace Infinity.Core.Matchmaking;

/// <summary>UDP_SOCKET_PROOF.md §6; routing hints confer no authority.</summary>
public sealed record NetworkCandidate
{
    public required string Kind { get; init; }
    public required string Ip { get; init; }
    public required ushort Port { get; init; }
    public override string ToString()=>"NetworkCandidate { redacted }";
}
public sealed record NetworkHints
{
    public required uint Revision { get; init; }
    public required IReadOnlyList<NetworkCandidate> Candidates { get; init; }
    public required string GeneratedAt { get; init; }
    public required string ExpiresAt { get; init; }
    public override string ToString()=>"NetworkHints { redacted }";
}
public sealed record SeatNetworkRequest
{
    public required string GrantId { get; init; }
    public required uint Revision { get; init; }
    public required AnnouncePort Endpoint { get; init; }
    public override string ToString()=>"SeatNetworkRequest { redacted }";
}
public sealed record SeatNetworkAnswer
{
    public required string GrantId { get; init; }
    public required string WorldId { get; init; }
    public required uint Revision { get; init; }
    public required IReadOnlyList<NetworkCandidate> Candidates { get; init; }
    public required string GeneratedAt { get; init; }
    public required string ExpiresAt { get; init; }
    public override string ToString()=>"SeatNetworkAnswer { redacted }";
}

internal static class RelayNetworkFrames
{
    internal static void Hint(ReadOnlySpan<byte> p)
    {
        if(p.Length!=25 || new Guid(p[..16],true)==Guid.Empty || BinaryPrimitives.ReadUInt32BigEndian(p[16..])==0
            || BinaryPrimitives.ReadUInt16BigEndian(p[20..])==0 || p[22]>2
            || (p[22]==0)!=(BinaryPrimitives.ReadUInt16BigEndian(p[23..])==0))
            throw new ProtocolException("Invalid relay network hint");
    }
    internal static bool Unicast(IPAddress ip)
        => !ip.IsIPv4MappedToIPv6 && (ip.AddressFamily==AddressFamily.InterNetwork
            ? ip.GetAddressBytes()[0] is >0 and <224 && !ip.Equals(IPAddress.Broadcast)
            : ip.AddressFamily==AddressFamily.InterNetworkV6 && !ip.Equals(IPAddress.IPv6Any) && !ip.IsIPv6Multicast && ip.ScopeId==0);

    internal static IPEndPoint[] Endpoints(IReadOnlyList<NetworkCandidate> candidates)
    {
        if(candidates is null || candidates.Count>5 || candidates.Any(c=>c is null) || candidates.Count(c=>c.Kind=="public")>1 || candidates.Count(c=>c.Kind=="lan")>4)
            throw new ProtocolException("Invalid relay candidate list");
        var result=new List<IPEndPoint>();
        foreach(var candidate in candidates)
        {
            if(candidate.Kind is not ("public" or "lan") || candidate.Port==0 || !IPAddress.TryParse(candidate.Ip,out var ip)
                || !Unicast(ip) || ip.ToString()!=candidate.Ip)throw new ProtocolException("Invalid relay candidate");
            var endpoint=new IPEndPoint(ip,candidate.Port);
            if(result.Contains(endpoint))throw new ProtocolException("Duplicate relay candidate");
            result.Add(endpoint);
        }
        return result.ToArray();
    }
    internal static byte[] Candidates(Guid grant,uint revision,byte ttl,IReadOnlyList<NetworkCandidate> candidates)
    {
        var endpoints=Endpoints(candidates);
        var p=new byte[22+20*endpoints.Length];grant.TryWriteBytes(p,true,out _);
        BinaryPrimitives.WriteUInt32BigEndian(p.AsSpan(16),revision);p[20]=ttl;p[21]=(byte)endpoints.Length;
        for(var i=0;i<endpoints.Length;i++)
        {
            var e=endpoints[i];var at=22+i*20;p[at]=candidates[i].Kind=="public"?(byte)0:(byte)1;
            p[at+1]=e.AddressFamily==AddressFamily.InterNetwork?(byte)4:(byte)6;
            e.Address.GetAddressBytes().CopyTo(p,at+2);BinaryPrimitives.WriteUInt16BigEndian(p.AsSpan(at+18),(ushort)e.Port);
        }
        ValidateCandidates(p);return p;
    }
    internal static void ValidateCandidates(ReadOnlySpan<byte> p)
    {
        if(p.Length<22 || new Guid(p[..16],true)==Guid.Empty || BinaryPrimitives.ReadUInt32BigEndian(p[16..])==0
            || p[20] is <1 or >30 || p[21]>5 || p.Length!=22+20*p[21])throw new ProtocolException("Invalid relay candidates");
        var list=new List<NetworkCandidate>();
        for(var i=0;i<p[21];i++)
        {
            var e=p.Slice(22+i*20,20);
            if(e[0]>1 || e[1] is not (4 or 6) || e[1]==4 && e.Slice(6,12).ContainsAnyExcept((byte)0))
                throw new ProtocolException("Invalid relay candidate address");
            list.Add(new(){Kind=e[0]==0?"public":"lan",Ip=new IPAddress(e.Slice(2,e[1]==4?4:16)).ToString(),Port=BinaryPrimitives.ReadUInt16BigEndian(e[18..])});
        }
        Endpoints(list);
    }
}
