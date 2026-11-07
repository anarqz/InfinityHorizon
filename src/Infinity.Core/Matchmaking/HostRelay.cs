// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// This PC's LAN addresses, which a seat's network hint carries (SeatNetworkRequest). OD-208: the launcher hosts no world, so the
/// former host relay's addressing is all that is left of it (the integrated relay and the Rust infinity-relay.exe are gone).
/// </summary>
public static class HostRelay
{
    /// <summary>
    /// This PC's private IPv4 addresses (RFC 1918), at most 4, for a seat's network hint (the FIRST one is used first), so the
    /// adapter with the default gateway comes first, and virtual
    /// adapters (Hyper-V / WSL vEthernet, VPNs, VirtualBox, VMware, TAP) are left out.
    /// </summary>
    public static IReadOnlyList<string> LanAddresses()
    {
        var ranked = new List<(bool Gateway, string Address)>();
        foreach (var nic in NetworkInterface.GetAllNetworkInterfaces())
        {
            if (nic.OperationalStatus != OperationalStatus.Up || IsVirtual(nic.NetworkInterfaceType, nic.Name + " " + nic.Description))
            {
                continue;
            }
            var props = nic.GetIPProperties();
            var gateway = props.GatewayAddresses.Any(g => g.Address.AddressFamily == AddressFamily.InterNetwork && !g.Address.Equals(IPAddress.Any));
            foreach (var unicast in props.UnicastAddresses)
            {
                if (unicast.Address.AddressFamily == AddressFamily.InterNetwork && IsPrivate(unicast.Address))
                {
                    ranked.Add((gateway, unicast.Address.ToString()));
                }
            }
        }
        return [.. ranked.OrderByDescending(r => r.Gateway).Select(r => r.Address).Distinct().Take(4)];
    }

    /// <summary>
    /// The address this PC reaches <paramref name="server"/> from: the local end of a UDP socket
    /// "connected" to it (no packet is sent; the OS just picks the route). Under Wine every Linux
    /// bridge looks like an ordinary adapter (SAURON: docker0, br-*), so this is the robust choice:
    /// when it is a private IPv4 it is this PC's only LAN address; otherwise the ranked list.
    /// </summary>
    public static IReadOnlyList<string> LanAddresses(Uri server)
    {
        ArgumentNullException.ThrowIfNull(server);
        try
        {
            using var probe = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp);
            var host = IPAddress.TryParse(server.Host, out var ip) ? ip
                : Dns.GetHostAddresses(server.Host).FirstOrDefault(a => a.AddressFamily == AddressFamily.InterNetwork);
            if (host is not null)
            {
                probe.Connect(new IPEndPoint(host, server.Port > 0 ? server.Port : 443));
                if (probe.LocalEndPoint is IPEndPoint local && IsPrivate(local.Address))
                {
                    return [local.Address.ToString()];
                }
            }
        }
        catch (Exception e) when (e is SocketException or ArgumentException)
        {
            // no route or no name: the ranked list
        }
        return LanAddresses();
    }

    /// <summary>An adapter no other PC on the LAN reaches this one through.</summary>
    public static bool IsVirtual(NetworkInterfaceType type, string nameAndDescription)
    {
        if (type is NetworkInterfaceType.Loopback or NetworkInterfaceType.Tunnel or NetworkInterfaceType.Ppp)
        {
            return true;
        }
        string[] virtualNames = ["Hyper-V", "vEthernet", "WSL", "VirtualBox", "VMware", "TAP-", "VPN", "Tailscale", "ZeroTier", "WireGuard", "Npcap", "Bluetooth"];
        return virtualNames.Any(n => (nameAndDescription ?? string.Empty).Contains(n, StringComparison.OrdinalIgnoreCase));
    }

    public static bool IsPrivate(IPAddress address)
    {
        ArgumentNullException.ThrowIfNull(address);
        var b = address.GetAddressBytes();
        return b.Length == 4 && (b[0] == 10 || (b[0] == 172 && b[1] >= 16 && b[1] <= 31) || (b[0] == 192 && b[1] == 168));
    }
}
