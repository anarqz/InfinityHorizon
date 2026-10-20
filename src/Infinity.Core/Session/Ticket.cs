// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;
using System.Text.Json.Serialization;
using Infinity.Core.Wire;
using Org.BouncyCastle.Math.EC.Rfc8032;

namespace Infinity.Core.Session;

/// <summary>
/// What the server signs for one launch (ACCOUNTS.md §4, launch ticket claims v1). Unknown fields
/// are ignored so the server can add fields without breaking older launchers and hooks.
/// </summary>
public sealed record TicketClaims
{
    [JsonPropertyName("v")] public required uint V { get; init; }

    /// <summary>Which launch-ticket key signed it (<c>lt-2026-10</c>); two keys overlap for 30 days.</summary>
    [JsonPropertyName("kid")] public required string Kid { get; init; }

    /// <summary>Game session id (server side).</summary>
    [JsonPropertyName("sid")] public required string Sid { get; init; }

    [JsonPropertyName("account_id")] public required string AccountId { get; init; }

    [JsonPropertyName("profile_id")] public required string ProfileId { get; init; }

    [JsonPropertyName("game")] public required string Game { get; init; }

    [JsonPropertyName("xuid")] public required ulong Xuid { get; init; }

    [JsonPropertyName("gamertag")] public required string Gamertag { get; init; }

    /// <summary><c>hardcore</c> or <c>noob</c>.</summary>
    [JsonPropertyName("mode")] public required string Mode { get; init; }

    /// <summary>Base URL the hook sends game traffic to.</summary>
    [JsonPropertyName("server")] public required string Server { get; init; }

    /// <summary><c>vip</c> or <c>free</c> (internal id; shown as "Infinity Supporter"). Display only: the server enforces it.</summary>
    [JsonPropertyName("tier")] public required string Tier { get; init; }

    /// <summary>Game-plane pool the ticket admits to (<c>vip</c>, <c>free</c>, <c>legacy-free</c>).</summary>
    [JsonPropertyName("pool")] public required string Pool { get; init; }

    /// <summary>Hex of the handoff nonce the launcher sent when it asked for the session.</summary>
    [JsonPropertyName("nonce")] public required string Nonce { get; init; }

    [JsonPropertyName("iat")] public required ulong Iat { get; init; }

    [JsonPropertyName("exp")] public required ulong Exp { get; init; }
}

/// <summary>The launch-ticket public keys embedded by kid: the current one and the next during a rotation.</summary>
public sealed class TicketKeys
{
    private readonly List<(string Kid, byte[] Key)> _keys = [];

    public TicketKeys(params (string Kid, byte[] Key)[] keys)
    {
        foreach (var (kid, key) in keys)
        {
            if (key.Length != 32)
            {
                throw new ArgumentException("an Ed25519 public key is 32 bytes", nameof(keys));
            }
            _keys.Add((kid, key.ToArray()));
        }
    }

    public byte[]? Get(string kid) => _keys.FirstOrDefault(k => k.Kid == kid).Key;

    /// <summary>The local stack's key id (compose <c>FI_LAUNCH_TICKET_SIGNING_KEYS</c>, <c>.env.example</c>).</summary>
    public const string LocalStackKid = "lt-local-1";

    /// <summary>
    /// The public half of <see cref="LocalStackKid"/> (supervisor.rs <c>LOCAL_STACK_KEY</c>, hook
    /// <c>--dev</c> <c>sign.c</c>). Its seed is public, so it is trusted only for a development server.
    /// </summary>
    public static ReadOnlySpan<byte> LocalStackKey =>
    [
        0x1f, 0xb7, 0xb2, 0x6e, 0xfc, 0x09, 0xb4, 0x08, 0xd2, 0xe6, 0x7f, 0xac, 0xc9, 0x6c, 0xf4, 0xfe,
        0x37, 0xb2, 0x09, 0x75, 0xaa, 0xae, 0xa0, 0x59, 0xab, 0xee, 0x40, 0x8b, 0x8c, 0xfd, 0xc4, 0x10,
    ];

    /// <summary><c>FI_DEV_TICKET_KEY=&lt;kid&gt;:&lt;64 hex&gt;</c>: one more key for a development server.</summary>
    public const string DevTicketKeyEnv = "FI_DEV_TICKET_KEY";

    /// <summary><c>&lt;kid&gt;:&lt;64 hex&gt;</c> (supervisor.rs <c>parse_dev_key</c>).</summary>
    public static (string Kid, byte[] Key)? ParseDevKey(string value)
    {
        ArgumentNullException.ThrowIfNull(value);
        var trimmed = value.Trim();
        var colon = trimmed.IndexOf(':', StringComparison.Ordinal);
        if (colon <= 0)
        {
            return null;
        }
        var hex = trimmed[(colon + 1)..];
        if (hex.Length != 64 || Hex.Decode(hex) is not { } key)
        {
            return null;
        }
        return (trimmed[..colon], key);
    }

    /// <summary>
    /// The keys and policy a launch ticket is verified with (supervisor.rs <c>ticket_trust</c>).
    /// Release builds embed no production key yet (ONLINE_OFFLINE T11), so only a development
    /// server (loopback or LAN, plain http) hands out a ticket the launcher accepts.
    /// </summary>
    public static (TicketKeys Keys, TicketPolicy Policy) Trust(bool developmentServer, string? devKeyEnv)
    {
        if (!developmentServer)
        {
            return (new TicketKeys(), TicketPolicy.Default);
        }
        var keys = new List<(string, byte[])> { (LocalStackKid, LocalStackKey.ToArray()) };
        if (devKeyEnv is not null && ParseDevKey(devKeyEnv) is { } extra)
        {
            keys.Add(extra);
        }
        return (new TicketKeys([.. keys]), TicketPolicy.Default with { AllowHttp = true });
    }
}

public sealed record TicketPolicy
{
    /// <summary>Allowed PC/server clock difference; the server enforces expiry, this only bounds stale tickets.</summary>
    public ulong MaxSkewSecs { get; init; } = 24 * 3600;

    public ulong MaxLifetimeSecs { get; init; } = 900;

    /// <summary>Development servers only (plain http on loopback or a private LAN address).</summary>
    public bool AllowHttp { get; init; }

    public static TicketPolicy Default { get; } = new();
}

public static class TicketVerifier
{
    /// <summary>Domain separation for the server's ticket signature.</summary>
    public static ReadOnlySpan<byte> SignPrefix => "fi-launch-ticket-v1\n"u8;

        // As serde: unknown fields ignored, but a null in a non-nullable field and a repeated key are
    // errors (a parser differential with the C hook, which verifies the same claims, must not exist).
    private static readonly JsonSerializerOptions Json = new()
    {
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Skip,
        RespectNullableAnnotations = true,
        AllowDuplicateProperties = false,
    };

    private static readonly JsonDocumentOptions KidJson = new() { AllowDuplicateProperties = false };

    /// <summary>
    /// The checks the hook performs (and the launcher before handing a ticket over; session.rs
    /// <c>verify_ticket</c>): the key named by kid, Ed25519 over <c>prefix || claims</c>, version 1,
    /// the launch's nonce, lifetime and skew bounds, and an https game server. The kid is read
    /// before the signature only to pick the key.
    /// </summary>
    public static TicketClaims Verify(byte[] claims, byte[] signature, TicketKeys keys, ReadOnlySpan<byte> nonce, ulong nowUnix, TicketPolicy policy)
    {
        ArgumentNullException.ThrowIfNull(claims);
        ArgumentNullException.ThrowIfNull(signature);
        ArgumentNullException.ThrowIfNull(keys);
        ArgumentNullException.ThrowIfNull(policy);
        var kid = ReadKid(claims) ?? throw new TicketException("claims carry no kid");
        var key = keys.Get(kid) ?? throw new TicketException($"unknown ticket key {kid}");
        if (signature.Length != 64 || !VerifyStrict(key, signature, claims))
        {
            throw new SignatureException();
        }
        TicketClaims parsed;
        try
        {
            parsed = JsonSerializer.Deserialize<TicketClaims>(claims, Json) ?? throw new TicketException("claims are null");
        }
        catch (Exception e) when (e is JsonException or InvalidOperationException)
        {
            throw new TicketException($"claims do not parse: {e.Message}");
        }
        if (parsed.V != 1)
        {
            throw new TicketException($"claims version {parsed.V}");
        }
        if (!string.Equals(parsed.Nonce, Hex.Encode(nonce), StringComparison.OrdinalIgnoreCase))
        {
            throw new TicketException("ticket belongs to another launch");
        }
        if (parsed.Exp <= parsed.Iat || parsed.Exp - parsed.Iat > policy.MaxLifetimeSecs)
        {
            throw new TicketException("ticket lifetime out of bounds");
        }
        if (parsed.Iat > SaturatingAdd(nowUnix, policy.MaxSkewSecs) || SaturatingAdd(parsed.Exp, policy.MaxSkewSecs) < nowUnix)
        {
            throw new TicketException("ticket expired");
        }
        var secure = parsed.Server.StartsWith("https://", StringComparison.Ordinal);
        var localHttp = policy.AllowHttp && DevNet.PlainHttpUrl(parsed.Server);
        if (!(secure || localHttp))
        {
            throw new TicketException("game server is not https");
        }
        return parsed;
    }

    private static ulong SaturatingAdd(ulong a, ulong b) => a > ulong.MaxValue - b ? ulong.MaxValue : a + b;

    private static string? ReadKid(byte[] claims)
    {
        try
        {
            using var doc = JsonDocument.Parse(claims, KidJson);
            return doc.RootElement.ValueKind == JsonValueKind.Object
                && doc.RootElement.TryGetProperty("kid", out var kid)
                && kid.ValueKind == JsonValueKind.String
                ? kid.GetString()
                : null;
        }
        catch (Exception e) when (e is JsonException or InvalidOperationException)
        {
            // InvalidOperationException: a lone surrogate in the kid (serde rejects it too).
            return null;
        }
    }

    /// <summary>
    /// Ed25519 as ed25519-dalek's <c>verify_strict</c>: canonical S (BouncyCastle checks it), and a
    /// public key that is a valid point outside the small-order torsion (<c>ValidatePublicKeyFull</c>).
    /// </summary>
    private static bool VerifyStrict(byte[] key, byte[] signature, byte[] claims)
    {
        if (!Ed25519.ValidatePublicKeyFull(key, 0))
        {
            return false;
        }
        var message = new byte[SignPrefix.Length + claims.Length];
        SignPrefix.CopyTo(message);
        claims.CopyTo(message, SignPrefix.Length);
        return Ed25519.Verify(signature, 0, key, 0, message, 0, message.Length);
    }
}
