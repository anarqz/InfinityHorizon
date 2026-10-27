// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Net;
using System.Net.Http.Json;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Text.Json;
using Infinity.Core.Api;
using Infinity.Core.Auth;
using Infinity.Core.WorldServer;

namespace Infinity.Core.Announce;

/// <summary>
/// One announced node (ANNOUNCER_API.md §3): <c>{publicKey, name, endpoint, region, version, capacity, firstSeen, lastSeen}</c>. Every field
/// is the announcer's own words: shown, never trusted (a server is still checked as any typed address, CR-L39).
/// </summary>
public sealed record Listing(string PublicKey, string Name, string Endpoint, string? Region, string? Version, int Capacity, string? FirstSeen, string? LastSeen);

/// <summary><c>GET /beholder/v1/servers</c> and <c>/relays</c>: <c>{items}</c>.</summary>
public sealed record ListingPage(IReadOnlyList<Listing> Items);

/// <summary>The lists the picker shows, and whether they came from the beholder now or from the copy saved earlier.</summary>
public sealed record BeholderLists(IReadOnlyList<Listing> Servers, IReadOnlyList<Listing> Relays, DateTimeOffset FetchedAt, bool Cached);

/// <summary>
/// OD122-L9 (owner: "um seletor de servidores e uma lista de relays"): the launcher reads the beholder, the public directory servers
/// and relays announce themselves to. Bounded (https only, 64 KiB per list, 500 items, 5 s), and the last good lists are kept in the
/// state root so the pickers still show something while the beholder is down.
/// </summary>
public sealed class BeholderClient(HttpClient http, Uri announcer)
{
    /// <summary>The release's default <c>announcement_server</c>.</summary>
    public static readonly Uri DefaultAnnouncer = new("https://beholder.infinity-horizon.com");

    public const string CacheFileName = "beholder-cache.json";
    public const int MaxItems = 500;
    public const int MaxBytes = 64 * 1024;
    public const int MaxName = 64;

    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);

    public Uri Announcer { get; } = announcer.Scheme == Uri.UriSchemeHttps || DevNet.PlainHttpUrl(announcer.GetLeftPart(UriPartial.Authority))
        ? announcer : throw new ArgumentException("The announcement server is https (plain http only on this PC or the LAN)", nameof(announcer));

    /// <summary>Both lists now, saved for later; on any failure the saved copy (marked cached), or empty lists.</summary>
    public async Task<BeholderLists> ListsAsync(string stateRoot, CancellationToken cancellation = default)
    {
        try
        {
            var servers = await ListAsync("servers", cancellation).ConfigureAwait(false);
            var relays = await ListAsync("relays", cancellation).ConfigureAwait(false);
            var lists = new BeholderLists(servers, relays, DateTimeOffset.UtcNow, Cached: false);
            try { Save(stateRoot, lists); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { } // a read-only state folder only loses the fallback
            return lists;
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or JsonException or InvalidDataException or IOException
            && !cancellation.IsCancellationRequested)
        {
            return Load(stateRoot) ?? new BeholderLists([], [], DateTimeOffset.MinValue, Cached: true);
        }
    }

    private async Task<IReadOnlyList<Listing>> ListAsync(string kind, CancellationToken cancellation)
    {
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        timeout.CancelAfter(TimeSpan.FromSeconds(5));
        using var response = await http.GetAsync(new Uri(Announcer, "/beholder/v1/" + kind), HttpCompletionOption.ResponseHeadersRead, timeout.Token).ConfigureAwait(false);
        response.EnsureSuccessStatusCode();
        if (response.Content.Headers.ContentLength is > MaxBytes) throw new InvalidDataException("The beholder's list is too large");
        await using var body = await response.Content.ReadAsStreamAsync(timeout.Token).ConfigureAwait(false);
        var bytes = new byte[MaxBytes + 1];
        var read = 0;
        for (int n; read < bytes.Length && (n = await body.ReadAsync(bytes.AsMemory(read), timeout.Token).ConfigureAwait(false)) > 0;) read += n;
        if (read > MaxBytes) throw new InvalidDataException("The beholder's list is too large");
        var page = JsonSerializer.Deserialize<ListingPage>(bytes.AsSpan(0, read), Json) ?? throw new InvalidDataException("The beholder answered no list");
        return Clean(page.Items ?? []);
    }

    /// <summary>Malformed rows dropped, names trimmed to printable text, at most <see cref="MaxItems"/>, one row per endpoint.</summary>
    public static IReadOnlyList<Listing> Clean(IEnumerable<Listing?> items) => items
        .Where(l => l is { PublicKey.Length: > 0 and <= 128, Endpoint.Length: > 0 and <= 255 })
        .Select(l => l! with { Name = Printable(l.Name, MaxName) is { Length: > 0 } name ? name : l.Endpoint, Region = Printable(l.Region, 32), Version = Printable(l.Version, 32) })
        .DistinctBy(l => l.Endpoint, StringComparer.OrdinalIgnoreCase)
        .Take(MaxItems)
        .ToList();

    private static string Printable(string? text, int max)
    {
        var kept = new string((text ?? "").Where(c => !char.IsControl(c) && !char.IsSurrogate(c) && c is not ('‎' or '‏' or '‪' or '‫' or '‬' or '‭' or '‮' or '⁦' or '⁧' or '⁨' or '⁩')).ToArray()).Trim();
        return kept.Length > max ? kept[..max] : kept;
    }

    private sealed record Saved(IReadOnlyList<Listing> Servers, IReadOnlyList<Listing> Relays, DateTimeOffset FetchedAt);

    private static void Save(string root, BeholderLists lists)
    {
        Directory.CreateDirectory(root);
        var path = Path.Combine(root, CacheFileName);
        File.WriteAllText(path + ".tmp", JsonSerializer.Serialize(new Saved(lists.Servers, lists.Relays, lists.FetchedAt), Json));
        File.Move(path + ".tmp", path, overwrite: true);
    }

    /// <summary>The lists saved by the last good read, marked cached; null when there are none.</summary>
    public static BeholderLists? Load(string root)
    {
        try
        {
            var path = Path.Combine(root, CacheFileName);
            if (!File.Exists(path) || new FileInfo(path).Length > 4 * MaxBytes) return null;
            var saved = JsonSerializer.Deserialize<Saved>(File.ReadAllText(path), Json);
            return saved is null ? null : new BeholderLists(Clean(saved.Servers ?? []), Clean(saved.Relays ?? []), saved.FetchedAt, Cached: true);
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return null;
        }
    }
}

/// <summary>
/// The official nodes, by their announce keys compiled into the build (a node's identity is its key; a compromised beholder cannot
/// promote anyone): a listing with another key is community, whatever its name says.
/// </summary>
public static class OfficialNodes
{
    /// <summary>"Infinity Sao Paulo 1" (the official relay on the Vultr VM).</summary>
    public static IReadOnlySet<string> Relays { get; } = new HashSet<string>(StringComparer.Ordinal) { "1EEeJWDweWn7PHOmTQ+j1ikK1IYLmftD8Vu9A5/AjJA=" };

    /// <summary>"Infinity Beta" (the owner's server behind https://play.infinity-horizon.com).</summary>
    public static IReadOnlySet<string> Servers { get; } = new HashSet<string>(StringComparer.Ordinal) { "n/JK6+KV3riBQMvX4LzQwUpW6hUxF0jNG+pEmsJN6Nw=" };

    public static bool IsOfficialRelay(Listing relay) => Relays.Contains(relay.PublicKey);

    public static bool IsOfficialServer(Listing server) => Servers.Contains(server.PublicKey);
}

/// <summary>How a listed node answered now: its round trip, or none (offline, refused, timed out).</summary>
public sealed record Reachability(TimeSpan? RoundTrip)
{
    public bool Online => RoundTrip is not null;
}

/// <summary>OD122-L9: a listed server's state (its <c>/launcher/v1/info</c> round trip) and a relay's (a STUN binding to its endpoint).</summary>
public static class NodeProbe
{
    /// <summary>A server's <c>/launcher/v1/info</c>, timed; a server whose address is not a valid custom server is never contacted.</summary>
    public static async Task<Reachability> ServerAsync(HttpClient http, string endpoint, string clientHeader, CancellationToken cancellation = default)
    {
        Uri origin;
        try { origin = CustomServer.ParseOrigin(endpoint); }
        catch (ArgumentException) { return new(null); }
        using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        timeout.CancelAfter(TimeSpan.FromSeconds(4));
        var clock = Stopwatch.StartNew();
        try
        {
            await ApiClient.InfoAsync(http, origin, clientHeader, timeout.Token).ConfigureAwait(false);
            return new(clock.Elapsed);
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or JsonException or InvalidDataException or ApiException or IOException
            && !cancellation.IsCancellationRequested)
        {
            return new(null);
        }
    }

    /// <summary>
    /// A relay's latency: up to three STUN binding requests to its <c>host:port</c> (IPv4 literal or name), 1 s each, the median of the
    /// answers whose transaction matches. No relay traffic beyond this (the fallback is a later CR).
    /// </summary>
    public static async Task<Reachability> RelayAsync(string endpoint, CancellationToken cancellation = default)
    {
        if (!TryEndpoint(endpoint, out var host, out var port)) return new(null);
        IPAddress? address;
        try
        {
            address = IPAddress.TryParse(host, out var literal) ? literal
                : (await Dns.GetHostAddressesAsync(host, AddressFamily.InterNetwork, cancellation).ConfigureAwait(false)).FirstOrDefault();
        }
        catch (SocketException) { return new(null); }
        if (address is null || address.AddressFamily != AddressFamily.InterNetwork) return new(null);
        using var udp = new UdpClient(AddressFamily.InterNetwork);
        var times = new List<TimeSpan>();
        for (var attempt = 0; attempt < 3; attempt++)
        {
            var transaction = RandomNumberGenerator.GetBytes(12);
            var clock = Stopwatch.StartNew();
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
            timeout.CancelAfter(TimeSpan.FromSeconds(1));
            try
            {
                await udp.SendAsync(StunCodec.BindingRequest(transaction), new IPEndPoint(address, port), timeout.Token).ConfigureAwait(false);
                while (true)
                {
                    var answer = await udp.ReceiveAsync(timeout.Token).ConfigureAwait(false);
                    if (StunCodec.Parse(answer.Buffer) is { Kind: StunCodec.BindingSuccessKind } message && message.Transaction.AsSpan().SequenceEqual(transaction))
                    {
                        times.Add(clock.Elapsed);
                        break;
                    }
                }
            }
            catch (OperationCanceledException) when (!cancellation.IsCancellationRequested) { }
            catch (SocketException) { }
        }
        if (times.Count == 0) return new(null);
        times.Sort();
        return new(times[times.Count / 2]);
    }

    /// <summary><c>host:port</c> with a port 1-65535 and an ASCII host.</summary>
    public static bool TryEndpoint(string endpoint, out string host, out int port)
    {
        host = ""; port = 0;
        var colon = endpoint.LastIndexOf(':');
        if (colon <= 0 || !int.TryParse(endpoint.AsSpan(colon + 1), System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out port)
            || port is < 1 or > 65535) return false;
        host = endpoint[..colon];
        return host.Length <= 253 && host.All(c => c is >= 'a' and <= 'z' or >= 'A' and <= 'Z' or >= '0' and <= '9' or '.' or '-');
    }
}

/// <summary>
/// OD122-L9: the relay the player chose (<c>relay-choice.json</c> in the state root): automatic (lowest latency) unless a listed relay's
/// endpoint was picked, and whether only official relays may be used. Nothing sends relay traffic yet (the fallback CR uses it).
/// </summary>
public static class RelayChoice
{
    public const string FileName = "relay-choice.json";

    public sealed record Saved(string? Endpoint, bool OfficialOnly);

    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);

    public static Saved Read(string root)
    {
        try
        {
            var path = Path.Combine(root, FileName);
            if (!File.Exists(path)) return new(null, false);
            var saved = JsonSerializer.Deserialize<Saved>(File.ReadAllText(path), Json) ?? new(null, false);
            return saved.Endpoint is { } endpoint && !NodeProbe.TryEndpoint(endpoint, out _, out _) ? saved with { Endpoint = null } : saved;
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return new(null, false);
        }
    }

    public static void Write(string root, Saved choice)
    {
        ArgumentNullException.ThrowIfNull(choice);
        Directory.CreateDirectory(root);
        var path = Path.Combine(root, FileName);
        File.WriteAllText(path + ".tmp", JsonSerializer.Serialize(choice, Json));
        File.Move(path + ".tmp", path, overwrite: true);
    }

    /// <summary>"Automatic": the reachable relay with the lowest round trip (allowed by <paramref name="allowed"/>), or null.</summary>
    public static Listing? Automatic(IEnumerable<(Listing Relay, Reachability State)> measured, Func<Listing, bool>? allowed = null) => measured
        .Where(m => m.State.RoundTrip is not null && (allowed?.Invoke(m.Relay) ?? true))
        .OrderBy(m => m.State.RoundTrip)
        .Select(m => m.Relay)
        .FirstOrDefault();
}
