// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;

namespace Infinity.Core.Presence;

/// <summary>
/// The Join lobby link (DISCORD_PRESENCE.md §4, CR-L41.2): <c>https://infinity-horizon.com/join/&lt;worldId&gt;</c> on the site, which
/// opens <c>infinity-horizon://join/&lt;worldId&gt;</c>. A world id is the client-minted UUID of MATCHMAKING_API.md §2.5, in its
/// canonical 8-4-4-4-12 hex form; the launcher accepts either link, or the bare id, and nothing else. The link grants nothing:
/// the join is the normal <c>POST /mm/v1/join {worldId}</c> with the player's own ticket.
/// </summary>
public static class JoinLink
{
    public const string Scheme = "infinity-horizon";
    public const string WebHost = "infinity-horizon.com";

    /// <summary>A canonical world id (36 characters, hex and hyphens at 8-13-18-23; either case).</summary>
    public static bool IsWorldId(string? value)
    {
        if (value is not { Length: 36 }) return false;
        for (var i = 0; i < 36; ++i)
        {
            var c = value[i];
            if (i is 8 or 13 or 18 or 23) { if (c != '-') return false; }
            else if (!char.IsAsciiHexDigit(c)) return false;
        }
        return true;
    }

    /// <summary>The world id of a pasted link or id (trimmed, lower case), or null.</summary>
    public static string? WorldId(string? text)
    {
        if (string.IsNullOrWhiteSpace(text) || text.Length > 512) return null;
        var value = text.Trim();
        if (IsWorldId(value)) return value.ToLowerInvariant();
        if (!Uri.TryCreate(value, UriKind.Absolute, out var uri) || !string.IsNullOrEmpty(uri.Query) || !string.IsNullOrEmpty(uri.Fragment)
            || !string.IsNullOrEmpty(uri.UserInfo)) return null;
        string path;
        if (uri.Scheme == Scheme)
        {
            // infinity-horizon://join/<id>: "join" is the authority
            if (!string.Equals(uri.Host, "join", StringComparison.OrdinalIgnoreCase) || !uri.IsDefaultPort && uri.Port != -1) return null;
            path = uri.AbsolutePath.Trim('/');
        }
        else if (uri.Scheme == Uri.UriSchemeHttps && uri.IsDefaultPort
            && (string.Equals(uri.Host, WebHost, StringComparison.OrdinalIgnoreCase) || string.Equals(uri.Host, "www." + WebHost, StringComparison.OrdinalIgnoreCase)))
        {
            var parts = uri.AbsolutePath.Trim('/').Split('/');
            if (parts.Length != 2 || parts[0] != "join") return null;
            path = parts[1];
        }
        else return null;
        return IsWorldId(path) ? path.ToLowerInvariant() : null;
    }

    /// <summary>The first <c>infinity-horizon://</c> argument of a launch (a protocol launch passes the link as <c>"%1"</c>).</summary>
    public static string? FromArguments(IEnumerable<string> arguments)
    {
        ArgumentNullException.ThrowIfNull(arguments);
        foreach (var argument in arguments)
            if (argument.StartsWith(Scheme + ":", StringComparison.OrdinalIgnoreCase)) return WorldId(argument);
        return null;
    }

    /// <summary>The site's Join lobby link for a world.</summary>
    public static string Web(string worldId) =>
        IsWorldId(worldId) ? "https://" + WebHost + "/join/" + worldId.ToLowerInvariant() : throw new ArgumentException("not a world id", nameof(worldId));

    /// <summary>The party id on the card: lowercase hex SHA-256 of the world id (never the id itself).</summary>
    public static string PartyId(string worldId) =>
        Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(worldId.ToLowerInvariant())));
}
