// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Concurrent;
using System.Text.Json;
using Infinity.Core.Api;
using Org.BouncyCastle.Math.EC.Rfc8032;

namespace Infinity.Core.Auth;

/// <summary>
/// CR-L39 (OD-122 local-first): a server the player chose besides the official one, <c>http://&lt;host&gt;:&lt;port&gt;</c> or https
/// on any host (owner: "Plain http anywhere"). Its launch tickets and relay grants verify with the keys its <c>/info</c> publishes
/// (<c>trust</c>, CR-S49), pinned per realm on first use. The official hostnames and realm are never a custom server.
/// </summary>
public static class CustomServer
{
    /// <summary>The official realm: a custom server presenting it is refused (its pins would mix with the official state).</summary>
    public const string OfficialRealm = "forzainfinity";

    /// <summary>The official Identity Platform project: its ID tokens open official accounts (a bearer token, about an hour).</summary>
    public const string OfficialIdentityProject = "forza-infinity-online";

    /// <summary>
    /// The owner's custom servers, the only ones that may receive an ID token of <see cref="OfficialIdentityProject"/> (OD-122
    /// follow-up "official accounts only on the owner's custom servers"): any other custom server could present it to the official
    /// server and enter that player's official account. OD122-L8: the live server moved off SAURON to the SteamOS notebook on the LAN
    /// and its Cloudflare tunnel.
    /// </summary>
    public static IReadOnlyList<Uri> OfficialIdentityServers { get; } = [new("http://192.168.68.57:8080/"), new(OwnerPublicOrigin)];

    /// <summary>
    /// OD122-L8: the owner's own server behind its Cloudflare tunnel, the one host under <c>infinity-horizon.com</c> that is a custom
    /// server (exactly this origin; the official service hosts never are). The hook allows the same origin in a custom ticket.
    /// </summary>
    public const string OwnerPublicOrigin = "https://play.infinity-horizon.com/";

    public static bool MayUseOfficialIdentity(Uri server) => OfficialIdentityServers.Contains(new Uri(server.GetLeftPart(UriPartial.Authority)));

    /// <summary>
    /// The origin of a player-typed address: http or https, an ASCII host name or IPv4 address (the hook takes nothing else), an
    /// optional port 1-65535, nothing more. No host under <c>infinity-horizon.com</c> is a custom server.
    /// </summary>
    public static Uri ParseOrigin(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        var trimmed = text.Trim().TrimEnd('/');
        if (!Uri.TryCreate(trimmed, UriKind.Absolute, out var uri) || uri.Scheme is not ("http" or "https"))
            throw new ArgumentException("A custom server is http:// or https:// followed by a host and an optional port");
        if (uri.UserInfo.Length != 0 || uri.Query.Length != 0 || uri.Fragment.Length != 0 || uri.AbsolutePath != "/" || uri.Host.Length == 0)
            throw new ArgumentException("A custom server is only a scheme, a host and a port");
        var host = uri.Host.TrimEnd('.');
        if (uri.HostNameType is not (UriHostNameType.Dns or UriHostNameType.IPv4) || host.Length == 0
            || !host.All(c => c is >= 'a' and <= 'z' or >= 'A' and <= 'Z' or >= '0' and <= '9' or '.' or '-') || uri.Port is <= 0 or > 65535)
            throw new ArgumentException("A custom server's host is an ASCII name or an IPv4 address with a port from 1 to 65535");
        var owners = uri.Scheme == "https" && uri.IsDefaultPort && host.Equals(new Uri(OwnerPublicOrigin).Host, StringComparison.OrdinalIgnoreCase);
        if (ProductionDeployment.IsOfficial(uri) || !owners && (host.Equals("infinity-horizon.com", StringComparison.OrdinalIgnoreCase)
            || host.EndsWith(".infinity-horizon.com", StringComparison.OrdinalIgnoreCase)))
            throw new ArgumentException("The official server is not a custom server");
        return new Uri(uri.GetLeftPart(UriPartial.Authority));
    }

    /// <summary>
    /// The custom server's <c>/info</c>: no official origin anywhere, a realm of its own, at least one launch-ticket key, and the
    /// official identity project only for <see cref="OfficialIdentityServers"/>.
    /// </summary>
    public static void ValidateInfo(ServerInfo info, Uri server)
    {
        ArgumentNullException.ThrowIfNull(info);
        ArgumentNullException.ThrowIfNull(server);
        if (info.ApiVersion != 1 || string.IsNullOrWhiteSpace(info.Realm) || info.Realm == OfficialRealm)
            throw new InvalidDataException("The custom server's discovery is invalid");
        foreach (var value in new[] { info.Services?.Core, info.Services?.Gate, info.Services?.Saves, info.Services?.Matchmaking, info.Idp?.EmulatorUrl })
        {
            if (string.IsNullOrEmpty(value)) continue;
            try { ParseOrigin(value); }
            catch (ArgumentException) { throw new InvalidDataException("The custom server's discovery names an invalid or official origin"); }
        }
        // the Auth emulator, as development builds take it: https, or plain http on loopback or the LAN
        if (info.Idp?.EmulatorUrl is { Length: > 0 } emulator && !emulator.StartsWith("https://", StringComparison.Ordinal)
            && !DevNet.PlainHttpUrl(new Uri(emulator).GetLeftPart(UriPartial.Authority)))
            throw new InvalidDataException("The custom server's Auth emulator is not https or a LAN address");
        if (info.Idp is { IsLocal: false, ProjectId: OfficialIdentityProject } && !MayUseOfficialIdentity(server))
            throw new OfficialIdentityRefusedException();
        if (info.Trust is not { LaunchTicket.Count: > 0 } trust)
            throw new InvalidDataException("The custom server publishes no launch-ticket key");
        Keys(trust.LaunchTicket);
        Keys(trust.RelayGrant ?? []);
    }

    /// <summary>
    /// The guard on the token itself (the advertised <c>projectId</c> proves nothing: the API key picks the project): an Identity
    /// Platform ID token whose audience is <see cref="OfficialIdentityProject"/>, or that cannot be read, never goes to a custom
    /// server outside <see cref="OfficialIdentityServers"/>.
    /// </summary>
    public static void RequireTokenFor(Uri server, string idToken)
    {
        ArgumentNullException.ThrowIfNull(server);
        ArgumentNullException.ThrowIfNull(idToken);
        if (MayUseOfficialIdentity(server)) return;
        if (Audience(idToken) is not { } audience || audience == OfficialIdentityProject) throw new OfficialIdentityRefusedException();
    }

    /// <summary>A JWT's <c>aud</c> (the payload is read, never trusted for anything else), or null when it is not one.</summary>
    internal static string? Audience(string token)
    {
        var parts = token.Split('.');
        if (parts.Length != 3 || parts[1].Length is 0 or > 8192) return null;
        try
        {
            var payload = parts[1].Replace('-', '+').Replace('_', '/');
            payload = payload.PadRight(payload.Length + (4 - payload.Length % 4) % 4, '=');
            using var json = JsonDocument.Parse(Convert.FromBase64String(payload));
            return json.RootElement.ValueKind == JsonValueKind.Object && json.RootElement.TryGetProperty("aud", out var aud)
                && aud.ValueKind == JsonValueKind.String ? aud.GetString() : null;
        }
        catch (Exception e) when (e is FormatException or JsonException)
        {
            return null;
        }
    }

    /// <summary>The origins a custom server's discovery points at, the server itself first: the plain-http allowance's scope.</summary>
    public static IEnumerable<Uri> Origins(ServerInfo info, Uri server)
    {
        ArgumentNullException.ThrowIfNull(info);
        yield return server;
        foreach (var value in new[] { info.Services?.Core, info.Services?.Gate, info.Services?.Saves, info.Services?.Matchmaking, info.Idp?.EmulatorUrl })
            if (!string.IsNullOrEmpty(value)) yield return ParseOrigin(value);
    }

    /// <summary><c>kid:hex</c> keys (CR-S49): a kid of lowercase letters, digits and dashes, 32 raw bytes, a valid Ed25519 point.</summary>
    public static (string Kid, byte[] Key)[] Keys(IEnumerable<string> entries)
    {
        ArgumentNullException.ThrowIfNull(entries);
        var keys = new List<(string Kid, byte[] Key)>();
        foreach (var entry in entries)
        {
            var colon = entry?.IndexOf(':', StringComparison.Ordinal) ?? -1;
            var kid = colon > 0 ? entry![..colon] : null;
            var hex = colon > 0 ? entry![(colon + 1)..] : null;
            if (kid is not { Length: <= 32 } || !kid.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-')
                || hex is not { Length: 64 } || !hex.All(c => c is >= '0' and <= '9' or >= 'a' and <= 'f'))
                throw new InvalidDataException("A custom server key is kid:64 lowercase hex");
            var key = Convert.FromHexString(hex);
            if (!Ed25519.ValidatePublicKeyFull(key, 0) || keys.Any(k => k.Kid == kid))
                throw new InvalidDataException("A custom server key is not a valid Ed25519 public key");
            keys.Add((kid, key));
        }
        if (keys.Count > 8) throw new InvalidDataException("A custom server publishes at most eight keys of a kind");
        return [.. keys];
    }
}

/// <summary>
/// Plain-http origins a custom server made acceptable (CR-L39): the server the player chose and the origins its validated
/// discovery names. <see cref="ApiClient.CheckOrigin"/> accepts http for exactly these; everything else keeps https or the
/// development LAN rule.
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

/// <summary>What a custom server's published keys and realm are against the ones pinned for its origin.</summary>
public enum TrustPin { FirstUse, Same, Changed }

/// <summary>
/// Trust on first use, per origin (CR-L39; the realm is the server's to say, so it is pinned with the keys, not the key):
/// <c>custom-trust.json</c> in that server's own state folder. A later change of keys or realm, or an unreadable file, is
/// <see cref="TrustPin.Changed"/> until the player accepts what the server now presents.
/// </summary>
public sealed class TrustPins(string path)
{
    private sealed record Pinned(string Realm, IReadOnlyList<string> LaunchTicket, IReadOnlyList<string>? RelayGrant);

    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web) { WriteIndented = true };
    private readonly object _sync = new();

    public string Path { get; } = path ?? throw new ArgumentNullException(nameof(path));

    public TrustPin Check(Uri origin, string realm, TrustKeys trust)
    {
        ArgumentNullException.ThrowIfNull(origin);
        ArgumentNullException.ThrowIfNull(trust);
        lock (_sync)
        {
            if (Read() is not { } pins) return TrustPin.Changed;
            if (!pins.TryGetValue(Key(origin), out var pinned)) return TrustPin.FirstUse;
            return pinned.Realm == realm && pinned.LaunchTicket.SequenceEqual(trust.LaunchTicket, StringComparer.Ordinal)
                && (pinned.RelayGrant ?? []).SequenceEqual(trust.RelayGrant ?? [], StringComparer.Ordinal) ? TrustPin.Same : TrustPin.Changed;
        }
    }

    /// <summary>Pins what <paramref name="origin"/> presents (first use, or the player accepted a change; an unreadable file is replaced).</summary>
    public void Pin(Uri origin, string realm, TrustKeys trust)
    {
        ArgumentNullException.ThrowIfNull(origin);
        ArgumentNullException.ThrowIfNull(trust);
        lock (_sync)
        {
            var pins = Read() ?? new(StringComparer.Ordinal);
            pins[Key(origin)] = new Pinned(realm, trust.LaunchTicket, trust.RelayGrant ?? []);
            Directory.CreateDirectory(System.IO.Path.GetDirectoryName(System.IO.Path.GetFullPath(Path))!);
            var temporary = Path + "." + Guid.NewGuid().ToString("N") + ".tmp";
            File.WriteAllText(temporary, JsonSerializer.Serialize(pins, Json));
            File.Move(temporary, Path, overwrite: true);
        }
    }

    private static string Key(Uri origin) => origin.GetLeftPart(UriPartial.Authority).ToLowerInvariant();

    /// <summary>The pins, or null when the file cannot be read (it never trusts silently).</summary>
    private Dictionary<string, Pinned>? Read()
    {
        if (!File.Exists(Path)) return new(StringComparer.Ordinal);
        try
        {
            return JsonSerializer.Deserialize<Dictionary<string, Pinned>>(File.ReadAllText(Path), Json) is { } pins
                && pins.Values.All(p => p?.Realm is not null && p.LaunchTicket is not null)
                ? new(pins, StringComparer.Ordinal) : null;
        }
        catch (JsonException)
        {
            return null;
        }
    }
}

/// <summary>
/// A custom server the build does not allow to receive the official identity project's tokens (OD-122 follow-up "official accounts only
/// on the owner's custom servers"): signing in there with an official account would hand it that account.
/// </summary>
/// <remarks>An <see cref="IOException"/>, as unusable discovery is everywhere else: the update read and session resume treat it so.</remarks>
public sealed class OfficialIdentityRefusedException() : IOException("This custom server may not receive official-account sign-ins");

/// <summary>A custom server presents other keys or another realm than the ones pinned for its origin; nothing proceeds until the player accepts.</summary>
public sealed class TrustChangedException(Uri origin, string realm, TrustKeys published) : Exception("The custom server's keys changed")
{
    public Uri Origin { get; } = origin;

    public string Realm { get; } = realm;

    public TrustKeys Published { get; } = published;

    /// <summary>A short, comparable fingerprint of the new launch-ticket keys for the confirmation.</summary>
    public string Fingerprint => string.Join(", ", Published.LaunchTicket.Select(k => k.Length > 16 ? k[..^48] + "…" : k));
}
