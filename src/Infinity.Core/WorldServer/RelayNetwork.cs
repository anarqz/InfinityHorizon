// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Net.Sockets;

namespace Infinity.Core.WorldServer;

public enum RelayNatKind { Unknown, Cone, Symmetric }
public enum RelayUpnpState { Off, Pending, NotFound, Mapped, Failed }

/// <summary>Immutable endpoint value; snapshots never expose a mutable IPEndPoint.</summary>
public sealed record RelayNetworkAddress(string Address, ushort Port);

/// <summary>Network discovery state for the launcher and hook. Contains no log or disk output.</summary>
/// <remarks>PunchHints/PunchesSent count granted peer hint revisions accepted and the datagrams handed to the socket
/// for them; SendErrors counts relay socket sends that failed. Diagnostics only, never routing.</remarks>
public sealed record RelayNetworkStatus(RelayNatKind Nat, IReadOnlyList<RelayNetworkAddress> PublicAddresses,
    RelayUpnpState Upnp, RelayNetworkAddress? UpnpAddress, bool BehindAnotherNat, bool DualStack,
    uint PunchHints = 0, uint PunchesSent = 0, uint SendErrors = 0);

/// <summary>Discovery is optional for isolated local tests; normal hosting enables STUN and UPnP.</summary>
public sealed record IntegratedRelayOptions
{
    private static readonly IReadOnlyList<string> DefaultServers = Array.AsReadOnly(new[] { "stun.l.google.com:19302", "stun.cloudflare.com:3478" });
    public bool EnableDiscovery { get; init; } = true;
    public bool EnableUpnp { get; init; } = true;
    public IReadOnlyList<string> StunServers { get; init; } = DefaultServers;
    public TimeSpan StunInterval { get; init; } = TimeSpan.FromSeconds(25);
    /// <summary>The host's installed game database (gamedbRC.slt, read-only): the events of an online Road Trip. Null: no Road Trip worlds.</summary>
    public string? GameDatabase { get; init; }

    /// <summary>The game database under a Mount folder, as the game maps it (the first of mediaoverride, mediapc, media); null when none.</summary>
    public static string? FindGameDatabase(string mount) =>
        Process.CarNames.MediaRoots.Select(root => Path.Combine(mount, root, "stripped", "gamedbRC.slt")).FirstOrDefault(File.Exists);
}

internal static class RelayNetwork
{
    /// <summary>Set dual mode before bind; an occupied port must never silently pick a new port.</summary>
    internal static Socket Bind(ushort port)
    {
        Socket? socket = null;
        try
        {
            socket = new Socket(AddressFamily.InterNetworkV6, SocketType.Dgram, ProtocolType.Udp) { DualMode = true, ExclusiveAddressUse = true };
            socket.Bind(new IPEndPoint(IPAddress.IPv6Any, port));
        }
        catch (SocketException exception) when (exception.SocketErrorCode != SocketError.AddressAlreadyInUse)
        {
            socket?.Dispose();
            socket = BindV4(port);
        }
        catch (NotSupportedException)
        {
            socket?.Dispose();
            socket = BindV4(port);
        }
        catch
        {
            socket?.Dispose();
            throw;
        }
        socket.Blocking = false;
        return socket;
    }

    private static Socket BindV4(ushort port)
    {
        var socket = new Socket(AddressFamily.InterNetwork, SocketType.Dgram, ProtocolType.Udp) { ExclusiveAddressUse = true };
        try { socket.Bind(new IPEndPoint(IPAddress.Any, port)); return socket; }
        catch { socket.Dispose(); throw; }
    }

    internal static RelayNetworkAddress Value(IPEndPoint endpoint) => new(endpoint.Address.ToString(), checked((ushort)endpoint.Port));

    internal static bool BehindAnotherNat(IPAddress address)
    {
        if (address.IsIPv4MappedToIPv6) address = address.MapToIPv4();
        if (address.AddressFamily != AddressFamily.InterNetwork) return false;
        var bytes = address.GetAddressBytes();
        return IPAddress.IsLoopback(address) || address.Equals(IPAddress.Any) || bytes[0] == 10
            || (bytes[0] == 172 && bytes[1] is >= 16 and <= 31) || (bytes[0] == 192 && bytes[1] == 168)
            || (bytes[0] == 169 && bytes[1] == 254) || (bytes[0] == 100 && bytes[1] is >= 64 and <= 127);
    }

    internal static bool RouterAddress(IPAddress address) => address.AddressFamily == AddressFamily.InterNetwork
        ? BehindAnotherNat(address) && !address.Equals(IPAddress.Any)
        : address.IsIPv6LinkLocal || address.IsIPv6UniqueLocal || IPAddress.IsLoopback(address);
}
