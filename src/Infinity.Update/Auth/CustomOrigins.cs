// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Concurrent;

namespace Infinity.Core.Auth;

/// <summary>
/// Plain-http origins a custom server made acceptable (CR-L39): the server the player chose and the origins its validated
/// discovery names. <see cref="Api.OriginPolicy.Check"/> (<c>ApiClient.CheckOrigin</c>) accepts http for exactly these; everything
/// else keeps https or the development LAN rule.
/// </summary>
public static class CustomOrigins
{
    private static readonly ConcurrentDictionary<string, byte> Allowed = new(StringComparer.OrdinalIgnoreCase);

    public static void Allow(Uri origin)
    {
        ArgumentNullException.ThrowIfNull(origin);
        Allowed[Key(origin)] = 0;
    }

    public static bool IsAllowed(Uri origin) => origin.IsAbsoluteUri && Allowed.ContainsKey(Key(origin));

    /// <summary>Tests only: a fresh process state.</summary>
    public static void Clear() => Allowed.Clear();

    private static string Key(Uri origin) => origin.Scheme + "://" + origin.Authority;
}
