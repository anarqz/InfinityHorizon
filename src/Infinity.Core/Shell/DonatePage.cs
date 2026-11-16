// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>
/// CR-L52 (owner 2026-11-15/16): the sidebar's DONATE opens the Ko-fi page. The page is <c>links.kofi</c> of
/// <c>GET /core/v1/me/support</c> when the server gives a usable one (the official server sets exactly <see cref="Fallback"/>);
/// otherwise (no session, the request failed, not https, or a self-hosted server that sets none) <see cref="Fallback"/>, with the
/// reason logged.
/// </summary>
public static class DonatePage
{
    /// <summary>The owner's Ko-fi page.</summary>
    public const string Fallback = "https://ko-fi.com/anarqz";

    /// <summary>The page to open for <paramref name="kofi"/>, and why it is <see cref="Fallback"/> (null: the server's own page).</summary>
    public static (Uri Page, string? FallbackReason) Choose(string? kofi)
    {
        if (string.IsNullOrWhiteSpace(kofi)) return (new Uri(Fallback), "the server sets no Ko-fi page");
        return Usable(kofi) is { } page ? (page, null) : (new Uri(Fallback), "the server's Ko-fi page is not an https address");
    }

    /// <summary>An absolute https address with a host and nothing hidden in it (no user info), or null.</summary>
    public static Uri? Usable(string? text) =>
        Uri.TryCreate(text?.Trim(), UriKind.Absolute, out var uri) && uri.Scheme == Uri.UriSchemeHttps && uri.Host.Length > 0
            && uri.UserInfo.Length == 0 && uri.HostNameType is UriHostNameType.Dns ? uri : null;
}
