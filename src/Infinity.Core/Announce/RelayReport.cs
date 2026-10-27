// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Announce;

/// <summary>
/// CR-S57 (MATCHMAKING_API.md §5b): what this launcher reports with a world it opens or a join: its NAT test (null: not measured),
/// its round trip to each reachable relay (announce key → ms, at most 16, 0-5000) and each relay's listed name (display only).
/// </summary>
public sealed record RelayReport(string? Nat, IReadOnlyDictionary<string, int> Relays, IReadOnlyDictionary<string, string> Names)
{
    public static RelayReport Empty { get; } = new(null, new Dictionary<string, int>(), new Dictionary<string, string>());
}

/// <summary>
/// Measures the beholder's relays for <see cref="RelayReport"/>: the player's relay choice (automatic: every reachable relay; a picked
/// one: only it; "official only" honoured), the picker's STUN round trip per relay, then the NAT test against the lowest one. Results
/// are kept 5 minutes (an empty one 30 s); concurrent callers share one measurement, bounded to <see cref="Budget"/>. OD-118: it is
/// only built for a signed-in play or dedicated session, and only runs when a world opens or before a join.
/// </summary>
public sealed class RelayReporter(
    Func<CancellationToken, Task<IReadOnlyList<Listing>>> relays,
    Func<RelayChoice.Saved> choice,
    TimeProvider? clock = null,
    Func<Listing, CancellationToken, Task<Reachability>>? probe = null,
    Func<Listing, CancellationToken, Task<string?>>? nat = null)
{
    public const int MaxRelays = 16;
    public const int MaxProbed = 32;
    public const int MaxMs = 5000;
    public static readonly TimeSpan CacheFor = TimeSpan.FromMinutes(5);
    public static readonly TimeSpan EmptyCacheFor = TimeSpan.FromSeconds(30);
    public static readonly TimeSpan Budget = TimeSpan.FromSeconds(6);

    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly Func<Listing, CancellationToken, Task<Reachability>> _probe = probe ?? ((relay, ct) => NodeProbe.RelayAsync(relay.Endpoint, ct));
    private readonly Func<Listing, CancellationToken, Task<string?>> _nat = nat ?? DefaultNatAsync;
    private readonly object _gate = new();
    private Task<RelayReport>? _running;
    private RelayReport? _cached;
    private long _cachedAt;

    /// <summary>The cached report while fresh, else a new measurement (never throws for network trouble: <see cref="RelayReport.Empty"/>).</summary>
    public Task<RelayReport> ReportAsync(CancellationToken cancellation = default)
    {
        lock (_gate)
        {
            if (_cached is { } cached && _clock.GetElapsedTime(_cachedAt) < (cached.Relays.Count == 0 ? EmptyCacheFor : CacheFor))
                return Task.FromResult(cached);
            _running ??= MeasureAsync();
            return _running.WaitAsync(cancellation);
        }
    }

    private static Task<string?> DefaultNatAsync(Listing relay, CancellationToken cancellation) =>
        NodeProbe.TryEndpoint(relay.Endpoint, out var host, out var binding)
            ? NodeProbe.NatAsync(host, binding, relay.RendezvousOrDefault(), cancellation: cancellation) : Task.FromResult<string?>(null);

    private async Task<RelayReport> MeasureAsync()
    {
        await Task.Yield();
        try
        {
            using var budget = new CancellationTokenSource(Budget, _clock);
            IReadOnlyList<Listing> listed;
            try { listed = await relays(budget.Token).ConfigureAwait(false); }
            catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException or InvalidDataException or JsonException
                or InvalidOperationException) { listed = []; }
            var saved = choice();
            var candidates = listed
                .Where(l => NodeProbe.TryEndpoint(l.Endpoint, out _, out _) && CanonicalKey(l.PublicKey)
                    && (!saved.OfficialOnly || OfficialNodes.IsOfficialRelay(l))
                    && (saved.Endpoint is null || string.Equals(l.Endpoint, saved.Endpoint, StringComparison.OrdinalIgnoreCase)))
                .DistinctBy(l => l.PublicKey, StringComparer.Ordinal)
                .OrderByDescending(OfficialNodes.IsOfficialRelay)
                .Take(MaxProbed)
                .ToList();
            var measured = await Task.WhenAll(candidates.Select(async relay => (Relay: relay, State: await ProbeAsync(relay, budget.Token).ConfigureAwait(false))))
                .ConfigureAwait(false);
            var reachable = measured.Where(m => m.State.RoundTrip is not null).OrderBy(m => m.State.RoundTrip).Take(MaxRelays).ToList();
            var map = reachable.ToDictionary(m => m.Relay.PublicKey,
                m => (int)Math.Clamp(Math.Round(m.State.RoundTrip!.Value.TotalMilliseconds), 0, MaxMs), StringComparer.Ordinal);
            var names = reachable.ToDictionary(m => m.Relay.PublicKey, m => m.Relay.Name, StringComparer.Ordinal);
            string? kind = null;
            if (reachable.Count > 0)
            {
                try { kind = await _nat(reachable[0].Relay, budget.Token).ConfigureAwait(false); }
                catch (Exception e) when (e is OperationCanceledException or System.Net.Sockets.SocketException or IOException) { }
            }
            var report = new RelayReport(kind, map, names);
            lock (_gate) { _cached = report; _cachedAt = _clock.GetTimestamp(); }
            return report;
        }
        finally { lock (_gate) _running = null; }
    }

    private async Task<Reachability> ProbeAsync(Listing relay, CancellationToken cancellation)
    {
        try { return await _probe(relay, cancellation).ConfigureAwait(false); }
        catch (Exception e) when (e is OperationCanceledException or System.Net.Sockets.SocketException or IOException) { return new(null); }
    }

    /// <summary>A beholder <c>publicKey</c> the server accepts in <c>relays</c>: canonical standard base64 of 32 bytes.</summary>
    public static bool CanonicalKey(string? key)
    {
        if (key is not { Length: 44 }) return false;
        try { return Convert.FromBase64String(key) is { Length: 32 } bytes && Convert.ToBase64String(bytes) == key; }
        catch (FormatException) { return false; }
    }
}
