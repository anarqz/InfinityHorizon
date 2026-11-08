// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Api;

/// <summary>The origin rule of every request and of the update's restart arguments (<c>ApiClient.CheckOrigin</c> is this check).</summary>
public static class OriginPolicy
{
    /// <summary>
    /// https anywhere; plain http only on loopback or a private LAN address with a port, or on an origin a custom server the
    /// player chose made acceptable (<see cref="Auth.CustomOrigins"/>, CR-L39).
    /// </summary>
    public static Uri Check(Uri origin)
    {
        ArgumentNullException.ThrowIfNull(origin);
        if (origin.Scheme == Uri.UriSchemeHttps)
        {
            return origin;
        }
        var text = origin.GetLeftPart(UriPartial.Authority);
        if (origin.Scheme == Uri.UriSchemeHttp && (DevNet.PlainHttpUrl(text) || Auth.CustomOrigins.IsAllowed(origin)))
        {
            return origin;
        }
        throw new ArgumentException($"{text} is not https, loopback or a private LAN address", nameof(origin));
    }
}
