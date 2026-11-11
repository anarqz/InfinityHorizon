// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.RegularExpressions;

namespace Infinity.Core.Diagnostics;

/// <summary>
/// OD-241: what leaves the PC in a "Get logs" zip carries no tokens, keys or raw packets. Every text file the collector adds goes through
/// <see cref="Redact"/>: credentials after their names (<c>token=</c>, <c>"refreshToken": "…"</c>, <c>Authorization: Bearer …</c>),
/// PASETO/JWT-shaped tokens, PEM keys, hex/base64 byte dumps, and the user name in home-folder paths.
/// </summary>
public static partial class LogRedactor
{
    public const string Mark = "<redacted>";

    public static string Redact(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        text = Pem().Replace(text, "<key removed>");
        text = Bearer().Replace(text, "$1" + Mark);
        text = Named().Replace(text, "$1" + Mark);
        text = Paseto().Replace(text, Mark);
        text = Jwt().Replace(text, Mark);
        text = HexBytes().Replace(text, "<bytes removed>");
        text = LongHex().Replace(text, "<bytes removed>");
        text = LongBase64().Replace(text, "<bytes removed>");
        text = WindowsHome().Replace(text, "$1<user>");
        text = UnixHome().Replace(text, "$1<user>");
        return text;
    }

    [GeneratedRegex(@"-----BEGIN [A-Z0-9 ]+-----[\s\S]*?(-----END [A-Z0-9 ]+-----|\z)")]
    private static partial Regex Pem();

    [GeneratedRegex(@"(?i)\b((?:bearer|basic)\s+)[A-Za-z0-9._~+/=-]{4,}")]
    private static partial Regex Bearer();

    // a credential's name, its separator (=, :, quotes), then the value up to a delimiter
    [GeneratedRegex(@"(?i)(""?[a-z0-9_.-]*(?:token|secret|password|passwd|pwd|api[_-]?key|site[_-]?key|private[_-]?key|access[_-]?key|signing[_-]?key|session[_-]?key|ticket|grant|proof|signature|cookie|authorization|credential)s?""?\s*[:=]\s*""?)(?!<redacted>|(?i:bearer|basic)\s)[^\s"",;&}\]]+")]
    private static partial Regex Named();

    [GeneratedRegex(@"\bv[1-4]\.(?:local|public)\.[A-Za-z0-9_\-]+(?:\.[A-Za-z0-9_\-]+)?")]
    private static partial Regex Paseto();

    [GeneratedRegex(@"\beyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]*")]
    private static partial Regex Jwt();

    // "0a 1b 2c ..." / "0a:1b:..." packet dumps of 16 bytes or more
    [GeneratedRegex(@"\b(?:[0-9A-Fa-f]{2}[ :]){15,}[0-9A-Fa-f]{2}\b")]
    private static partial Regex HexBytes();

    // continuous hex longer than a SHA-512 (hashes in the logs stay; raw payloads do not)
    [GeneratedRegex(@"\b[0-9A-Fa-f]{129,}\b")]
    private static partial Regex LongHex();

    [GeneratedRegex(@"[A-Za-z0-9+/_-]{160,}={0,2}")]
    private static partial Regex LongBase64();

    // C:\Users\<name>, and Z:\home\<name> (the Linux home through Wine's Z: drive)
    [GeneratedRegex(@"(?i)([A-Z]:[\\/]+(?:Users|home)[\\/]+)(?!<user>)[^\\/\s""']+")]
    private static partial Regex WindowsHome();

    [GeneratedRegex(@"((?:^|[\s""'=(])/home/)(?!<user>)[^/\s""']+")]
    private static partial Regex UnixHome();
}
