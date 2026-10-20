// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;

namespace Infinity.Core;

/// <summary>
/// Where plain <c>http://</c> is accepted (development and LAN test sessions, OD-34; devnet.rs):
/// <c>127.0.0.1</c> and private IPv4 LAN addresses (10/8, 172.16/12, 192.168/16). Everything else
/// needs https.
/// </summary>
public static class DevNet
{
    /// <summary><paramref name="host"/> (no port) is <c>127.0.0.1</c> or a private IPv4 LAN address.</summary>
    public static bool PlainHttpHost(string host)
    {
        if (ParseIpv4(host) is not { } ip)
        {
            return false;
        }
        return (ip[0] == 127 && ip[1] == 0 && ip[2] == 0 && ip[3] == 1)
            || ip[0] == 10
            || (ip[0] == 172 && ip[1] >= 16 && ip[1] <= 31)
            || (ip[0] == 192 && ip[1] == 168);
    }

    /// <summary><paramref name="url"/> is <c>http://&lt;plain host&gt;:&lt;port&gt;[/...]</c> with a canonical port.</summary>
    public static bool PlainHttpUrl(string url)
    {
        ArgumentNullException.ThrowIfNull(url);
        if (!url.StartsWith("http://", StringComparison.Ordinal))
        {
            return false;
        }
        var rest = url["http://".Length..];
        var slash = rest.IndexOf('/', StringComparison.Ordinal);
        var authority = slash < 0 ? rest : rest[..slash];
        var colon = authority.LastIndexOf(':');
        if (colon < 0)
        {
            return false;
        }
        var port = authority[(colon + 1)..];
        return PlainHttpHost(authority[..colon])
            && ushort.TryParse(port, NumberStyles.None, CultureInfo.InvariantCulture, out var p)
            && p > 0
            && port == p.ToString(CultureInfo.InvariantCulture);
    }

    /// <summary>A strict dotted quad as Rust's <c>Ipv4Addr</c> parses it: four decimal octets, no leading zeros.</summary>
    private static byte[]? ParseIpv4(string text)
    {
        var parts = text.Split('.');
        if (parts.Length != 4)
        {
            return null;
        }
        var octets = new byte[4];
        for (var i = 0; i < 4; i++)
        {
            var part = parts[i];
            if (part.Length is 0 or > 3 || (part.Length > 1 && part[0] == '0'))
            {
                return null;
            }
            if (!byte.TryParse(part, NumberStyles.None, CultureInfo.InvariantCulture, out octets[i]))
            {
                return null;
            }
        }
        return octets;
    }
}
