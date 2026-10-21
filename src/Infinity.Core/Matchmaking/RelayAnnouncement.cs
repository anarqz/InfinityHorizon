// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Net.Sockets;
using Infinity.Core.WorldServer;

namespace Infinity.Core.Matchmaking;

internal static class RelayAnnouncement
{
    internal static AnnouncePort Build(ushort localPort,IReadOnlyList<string>? lan,RelayNetworkStatus? network)
    {
        AnnounceExternalPort? external=null;
        if(network is {Upnp:RelayUpnpState.Mapped,BehindAnotherNat:false,UpnpAddress:{Port:>0} mapped} && Public(mapped))
            external=new(){Port=mapped.Port,Source="upnp"};
        else if(network is {Nat:RelayNatKind.Cone} && network.PublicAddresses.FirstOrDefault(a=>Public(a)) is {Port:>0} observed)
            external=new(){Port=observed.Port,Source="stun"};
        return new(){Port=localPort,Lan=lan is {Count:>0}?lan:null,External=external};
    }
    private static bool Public(RelayNetworkAddress address)=>IPAddress.TryParse(address.Address,out var ip)
        && ip.AddressFamily==AddressFamily.InterNetwork && UdpProofCodec.Unicast(ip) && !IPAddress.IsLoopback(ip) && !HostRelay.IsPrivate(ip);
}
