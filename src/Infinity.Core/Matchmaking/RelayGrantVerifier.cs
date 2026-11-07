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
    /// <summary>CR-S57: the signed relay both sides meet at (the last claim, absent when none). The only relay ever contacted.</summary>
    [JsonPropertyName("relay"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public RelayGrantRelay? Relay { get; init; }
    /// <summary>
    /// CR-S67: the joiner account's voice block in force when the grant was signed, in Unix seconds (the last claim, after
    /// <see cref="Relay"/>; absent when no block is in force, so an unblocked grant's bytes are unchanged).
    /// </summary>
    [JsonPropertyName("voiceBlockedUntil"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public long? VoiceBlockedUntil { get; init; }
}

/// <summary>
/// Exact fi_protocol::matchmaking::RelayGrantHost mirror (WORLDS_OFFICIAL.md §3): <c>{xuid}</c> (a player host's, the contract's) or
/// <c>{official: vmId}</c> (a Worlds host). OD-208: this client verifies only the official one; a player host's is refused.
/// </summary>
public sealed record RelayGrantHost
{
    [JsonPropertyName("xuid"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public string? Xuid { get; init; }
    [JsonPropertyName("official"), JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)] public string? Official { get; init; }
}

/// <summary>
/// Known world and peer identity from the authenticated control plane, never from the UDP sender. The host is
/// <paramref name="HostXuid"/> for a player-hosted world, or <paramref name="HostOfficial"/> (with a null xuid) for an official one.
/// </summary>
public sealed record RelayGrantContext(string WorldId, string? HostXuid, string PeerXuid, string Role,
    string? GrantId = null, string? ProfileId = null, string? HostOfficial = null);

/// <summary>WORLDS_OFFICIAL.md §3: an official Worlds host's <c>vmId</c> and the FIUP host tag the launcher and the engine derive from it.</summary>
public static class OfficialHost
{
    public static ReadOnlySpan<byte> TagPrefix => "fi-official-host-v1\n"u8;

    /// <summary><c>[a-z0-9-]{1,32}</c>, the <c>FI_WORLDS</c> entry's id.</summary>
    public static bool ValidVmId(string? vmId) => RelayGrantVerifyKeys.ValidKid(vmId);

    /// <summary>
    /// <c>0x8000000000000000 | BE64(SHA-256("fi-official-host-v1\n" ‖ vmId)[0..8])</c>: nonzero, top bit set (outside every real
    /// XUID). Only the FIUP header's host field carries it, never a claim, row or log as a player identity.
    /// </summary>
    public static ulong Tag(string vmId)
    {
        if (!ValidVmId(vmId)) throw new ArgumentException("Invalid official host id", nameof(vmId));
        Span<byte> digest = stackalloc byte[32];
        System.Security.Cryptography.SHA256.HashData([.. TagPrefix, .. System.Text.Encoding.UTF8.GetBytes(vmId)], digest);
        return 0x8000000000000000UL | System.Buffers.Binary.BinaryPrimitives.ReadUInt64BigEndian(digest);
    }

    /// <summary>The FIUP host field of a verified grant's host: the XUID, or the official tag.</summary>
    public static ulong HostField(RelayGrantHost host)
    {
        ArgumentNullException.ThrowIfNull(host);
        return host.Official is { } vmId ? Tag(vmId) : throw new InvalidDataException(); // OD-208: an official host only
    }
}

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

    /// <summary>The ring in <see cref="Parse"/>'s form, <c>kid:base64,…</c> (public keys only; the world server's <c>--grant-keys</c>).</summary>
    public string Ring() => string.Join(",", _keys.OrderBy(k => k.Key, StringComparer.Ordinal).Select(k => k.Key + ":" + Convert.ToBase64String(k.Value)));

    /// <summary>
    /// The local stack's relay-grant PUBLIC ring (compose/matchmaking.yml's
    /// <c>FI_RELAY_GRANT_VERIFY_KEYS</c> default, the public half of its public dev seed). Only for
    /// a private/loopback plain-http stack; never a production key.
    /// </summary>
#if !INFINITY_PRODUCTION_BUILD
    public const string LocalStack = "rg-local-1:A4Qtlcgz2EIO20E32DMyuO5ss4tu84VRWw51EeB04wg=";
#endif

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
#if !INFINITY_PRODUCTION_BUILD
        if (string.IsNullOrEmpty(origin.UserInfo) && origin.Query.Length == 0 && origin.Fragment.Length == 0
            && DevNet.PlainHttpUrl(origin.OriginalString) && developmentKeys is not null)
            keys.AddRange(developmentKeys.Take(9));
#endif
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
            || !Xuid(verified.Xuid) || !ValidHost(verified)
            || verified.Relay is not null || verified.VoiceBlockedUntil is < 0) throw Invalid(); // OD-208: official worlds are direct
        if (verified.Exp <= nowUnix) throw new RelayGrantException(RelayGrantFailure.Expired);
        if (!string.Equals(verified.World, context.WorldId, StringComparison.Ordinal)
            || !string.Equals(verified.Host.Xuid, context.HostXuid, StringComparison.Ordinal)
            || !string.Equals(verified.Host.Official, context.HostOfficial, StringComparison.Ordinal)
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

    /// <summary>
    /// WORLDS_OFFICIAL.md §3, OD-208: only <c>host: {official: vmId}</c>. An official host's grant is a joiner's (the engine is the
    /// host) and carries no <c>relay</c> claim (official worlds are direct, OD-189); a player host's <c>{xuid}</c> is refused.
    /// </summary>
    private static bool ValidHost(RelayGrantClaims claims) => claims.Host is { Xuid: null, Official: { } vmId }
        && OfficialHost.ValidVmId(vmId) && claims.Role == "joiner" && claims.Relay is null;

    private static bool Uuid(string value) => Guid.TryParseExact(value, "D", out var id) && id != Guid.Empty
        && string.Equals(id.ToString("D"), value, StringComparison.Ordinal);
    private static bool Xuid(string value) => ulong.TryParse(value, NumberStyles.None, CultureInfo.InvariantCulture, out var id)
        && id != 0 && string.Equals(id.ToString(CultureInfo.InvariantCulture), value, StringComparison.Ordinal);
    private static RelayGrantException Invalid() => new(RelayGrantFailure.Invalid);
}
