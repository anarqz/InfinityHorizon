// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Serialization;
using Org.BouncyCastle.Math.EC.Rfc8032;

namespace Infinity.Core.Matchmaking;

/// <summary>Exact fi_protocol::matchmaking::RelayGrantClaims mirror, including its signed field names.</summary>
public sealed record RelayGrantClaims
{
    [JsonPropertyName("v")] public required uint V { get; init; }
    [JsonPropertyName("kid")] public required string Kid { get; init; }
    [JsonPropertyName("grant")] public required string Grant { get; init; }
    [JsonPropertyName("world")] public required string World { get; init; }
    [JsonPropertyName("sub")] public required string Sub { get; init; }
    [JsonPropertyName("xuid")] public required string Xuid { get; init; }
    [JsonPropertyName("tier")] public required string Tier { get; init; }
    [JsonPropertyName("host")] public required RelayGrantHost Host { get; init; }
    [JsonPropertyName("role")] public required string Role { get; init; }
    [JsonPropertyName("iat")] public required long Iat { get; init; }
    [JsonPropertyName("exp")] public required long Exp { get; init; }
}

/// <summary>Exact fi_protocol::matchmaking::RelayGrantHost mirror.</summary>
public sealed record RelayGrantHost
{
    [JsonPropertyName("xuid")] public required string Xuid { get; init; }
}

/// <summary>Known world and peer identity from the authenticated control plane, never from the UDP sender.</summary>
public sealed record RelayGrantContext(string WorldId, string HostXuid, string PeerXuid, string Role,
    string? GrantId = null, string? ProfileId = null);

public enum RelayGrantFailure { Invalid, Expired, ContextMismatch }

/// <summary>Failure categories carry no raw claims, signatures or player data.</summary>
public sealed class RelayGrantException(RelayGrantFailure failure) : Exception("Relay grant verification failed")
{
    public RelayGrantFailure Failure { get; } = failure;
}

/// <summary>Relay-grant public keys only. No launch-ticket key can enter this ring implicitly.</summary>
public sealed class RelayGrantVerifyKeys
{
    private readonly Dictionary<string, byte[]> _keys = new(StringComparer.Ordinal);

    /// <summary>An empty ring is fail-closed until trusted release public keys are supplied.</summary>
    public RelayGrantVerifyKeys(params (string Kid, byte[] Key)[] keys)
    {
        ArgumentNullException.ThrowIfNull(keys);
        if (keys.Length > 8) throw new ArgumentException("At most eight relay public keys may be configured", nameof(keys));
        foreach (var (kid, key) in keys)
        {
            if (!ValidKid(kid) || key is not { Length: 32 } || !Ed25519.ValidatePublicKeyFull(key, 0)
                || !_keys.TryAdd(kid, key.ToArray()))
                throw new ArgumentException("Invalid or duplicate relay public key", nameof(keys));
        }
    }

    public int Count => _keys.Count;

    /// <summary>
    /// The local stack's relay-grant PUBLIC ring (compose/matchmaking.yml's
    /// <c>FI_RELAY_GRANT_VERIFY_KEYS</c> default, the public half of its public dev seed). Only for
    /// a private/loopback plain-http stack; never a production key.
    /// </summary>
    public const string LocalStack = "rg-local-1:A4Qtlcgz2EIO20E32DMyuO5ss4tu84VRWw51EeB04wg=";

    internal static bool ValidKid(string? kid) => kid is { Length: >= 1 and <= 32 }
        && kid.All(character => character is >= 'a' and <= 'z' or >= '0' and <= '9' or '-');

    /// <summary>Parses the existing FI_RELAY_GRANT_VERIFY_KEYS format; this method reads no environment.</summary>
    public static RelayGrantVerifyKeys Parse(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        if (text.Length > 1024) throw new ArgumentException("Relay public key configuration is too long", nameof(text));
        var pairs = new List<(string Kid, byte[] Key)>();
        foreach (var item in text.Split(',', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries))
        {
            var separator = item.IndexOf(':');
            if (separator < 1) throw new ArgumentException("A relay public key is kid:base64", nameof(text));
            var encoded = item[(separator + 1)..].Trim();
            byte[] key;
            try { key = Convert.FromBase64String(encoded); }
            catch (FormatException) { throw new ArgumentException("A relay public key is standard base64", nameof(text)); }
            if (key.Length != 32 || Convert.ToBase64String(key) != encoded)
                throw new ArgumentException("A relay public key is standard base64 of 32 bytes", nameof(text));
            pairs.Add((item[..separator].Trim(), key));
        }
        if (pairs.Count == 0) throw new ArgumentException("No relay public key is configured", nameof(text));
        return new([.. pairs]);
    }

    /// <summary>Development keys are added only for literal private/loopback HTTP development origins.</summary>
    public static RelayGrantVerifyKeys Trust(Uri origin, IEnumerable<(string Kid, byte[] Key)>? releaseKeys = null,
        IEnumerable<(string Kid, byte[] Key)>? developmentKeys = null)
    {
        ArgumentNullException.ThrowIfNull(origin);
        if (!origin.IsAbsoluteUri) throw new ArgumentException("An absolute server origin is required", nameof(origin));
        var keys = releaseKeys?.Take(9).ToList() ?? [];
        if (string.IsNullOrEmpty(origin.UserInfo) && origin.Query.Length == 0 && origin.Fragment.Length == 0
            && DevNet.PlainHttpUrl(origin.OriginalString) && developmentKeys is not null)
            keys.AddRange(developmentKeys.Take(9));
        return new([.. keys]);
    }

    internal bool Verify(string kid, byte[] signature, byte[] message) => _keys.TryGetValue(kid, out var key)
        && Ed25519.ValidatePublicKeyFull(key, 0) && Ed25519.Verify(signature, 0, key, 0, message, 0, message.Length);
}

/// <summary>Offline Ed25519 verification of the exact signed bytes and authenticated hosting context.</summary>
public static class RelayGrantVerifier
{
    public const int MaxClaimsBytes = 1024;
    public const long MaxLifetimeSeconds = 60;
    public const long FutureSkewSeconds = 30;
    public static ReadOnlySpan<byte> SignPrefix => "fi-relay-grant-v1\n"u8;

    private static readonly JsonSerializerOptions Json = new()
    {
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
        RespectNullableAnnotations = true,
        AllowDuplicateProperties = false,
        MaxDepth = 4,
    };
    private static readonly JsonDocumentOptions KidJson = new() { AllowDuplicateProperties = false, MaxDepth = 4 };

    public static RelayGrantClaims Verify(RelayGrant grant, RelayGrantVerifyKeys keys, long nowUnix, RelayGrantContext context)
    {
        ArgumentNullException.ThrowIfNull(grant);
        return VerifyRaw(Decode(grant.Claims, MaxClaimsBytes), Decode(grant.Signature, 64), keys, nowUnix, context);
    }

    /// <summary>Only kid selects a key before signature verification. Claims are never reserialized to verify.</summary>
    public static RelayGrantClaims VerifyRaw(ReadOnlySpan<byte> claims, ReadOnlySpan<byte> signature,
        RelayGrantVerifyKeys keys, long nowUnix, RelayGrantContext context)
    {
        ArgumentNullException.ThrowIfNull(keys);
        ArgumentNullException.ThrowIfNull(context);
        ArgumentOutOfRangeException.ThrowIfNegative(nowUnix);
        if (claims.Length is 0 or > MaxClaimsBytes || signature.Length != 64) throw Invalid();
        var bytes = claims.ToArray();
        string? kid;
        try
        {
            using var parsed = JsonDocument.Parse(bytes, KidJson);
            kid = parsed.RootElement.ValueKind == JsonValueKind.Object && parsed.RootElement.TryGetProperty("kid", out var key)
                && key.ValueKind == JsonValueKind.String ? key.GetString() : null;
        }
        catch (Exception exception) when (exception is JsonException or InvalidOperationException) { throw Invalid(); }
        if (!RelayGrantVerifyKeys.ValidKid(kid)) throw Invalid();
        byte[] message = [.. SignPrefix, .. bytes];
        if (!keys.Verify(kid!, signature.ToArray(), message)) throw Invalid();
        RelayGrantClaims verified;
        try { verified = JsonSerializer.Deserialize<RelayGrantClaims>(bytes, Json) ?? throw Invalid(); }
        catch (Exception exception) when (exception is JsonException or InvalidOperationException) { throw Invalid(); }
        if (verified.V != 1 || verified.Role is not ("host" or "joiner") || verified.Tier is not ("free" or "vip")
            || verified.Exp <= verified.Iat || (Int128)verified.Exp - verified.Iat > MaxLifetimeSeconds
            || (Int128)verified.Iat - nowUnix > FutureSkewSeconds
            || !Uuid(verified.Grant) || !Uuid(verified.World) || !Uuid(verified.Sub)
            || !Xuid(verified.Xuid) || !Xuid(verified.Host.Xuid)) throw Invalid();
        if (verified.Exp <= nowUnix) throw new RelayGrantException(RelayGrantFailure.Expired);
        if (!string.Equals(verified.World, context.WorldId, StringComparison.Ordinal)
            || !string.Equals(verified.Host.Xuid, context.HostXuid, StringComparison.Ordinal)
            || !string.Equals(verified.Xuid, context.PeerXuid, StringComparison.Ordinal)
            || !string.Equals(verified.Role, context.Role, StringComparison.Ordinal)
            || context.GrantId is { } grantId && !string.Equals(verified.Grant, grantId, StringComparison.Ordinal)
            || context.ProfileId is { } profileId && !string.Equals(verified.Sub, profileId, StringComparison.Ordinal))
            throw new RelayGrantException(RelayGrantFailure.ContextMismatch);
        return verified;
    }

    private static byte[] Decode(string? text, int maximum)
    {
        if (text is null || text.Length == 0 || text.Length > (maximum + 2) / 3 * 4) throw Invalid();
        try
        {
            var bytes = Convert.FromBase64String(text);
            if (bytes.Length > maximum || Convert.ToBase64String(bytes) != text) throw Invalid();
            return bytes;
        }
        catch (FormatException) { throw Invalid(); }
    }

    private static bool Uuid(string value) => Guid.TryParseExact(value, "D", out var id) && id != Guid.Empty
        && string.Equals(id.ToString("D"), value, StringComparison.Ordinal);
    private static bool Xuid(string value) => ulong.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out var id)
        && id != 0 && string.Equals(id.ToString(CultureInfo.InvariantCulture), value, StringComparison.Ordinal);
    private static RelayGrantException Invalid() => new(RelayGrantFailure.Invalid);
}
