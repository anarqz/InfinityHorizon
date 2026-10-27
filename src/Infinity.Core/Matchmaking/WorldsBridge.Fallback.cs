// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Concurrent;
using System.Net;
using System.Security.Cryptography;
using Infinity.Core.Announce;
using Infinity.Core.Session;

namespace Infinity.Core.Matchmaking;

/// <summary>
/// CR-S57 relay fallback (MATCHMAKING_API.md §5b), the launcher's half: the NAT/relay report on open and join, the
/// <see cref="Frame.RelayLeg"/> pushed beside every <see cref="Frame.RelayContext"/>, and the hook's
/// <see cref="Frame.RelayFallback"/> answered with an immediate <c>transport: "relay"</c> renew.
/// </summary>
public sealed partial class WorldsBridge
{
    /// <summary>The hook reported <see cref="CaptureWhat.RelayJoiner"/> ok in this session: joins send <c>nat</c>/<c>relays</c>, legs are pushed.</summary>
    public bool RelayJoinerCapable { get => Volatile.Read(ref _relayJoinerCapable); set => Volatile.Write(ref _relayJoinerCapable, value); }
    private bool _relayJoinerCapable;

    /// <summary>Writes a leg to the hook (the supervisor's control pipe).</summary>
    public Func<Frame.RelayLeg, CancellationToken, Task>? SendRelayLeg { get; set; }

    /// <summary>The relay report (<see cref="RelayReporter"/>); null: none is measured (the host sends neither field).</summary>
    public Func<CancellationToken, Task<RelayReport>>? RelayReports { get; init; }

    public static readonly TimeSpan RelayReportBound = TimeSpan.FromSeconds(8);

    /// <summary>
    /// <see cref="RelayForce"/> (<c>FI_RELAY_FORCE=1</c>, a test switch): a joined grant that names a relay goes there at once, its
    /// leg relay-born and its seat renewed with <c>transport: "relay"</c> (the path of the hook's direct failure). Hosting is unchanged.
    /// </summary>
    public bool ForceRelay { get; init; }

    /// <summary>After a granted join under <see cref="ForceRelay"/>: the immediate relay renew, or a log line and direct play.</summary>
    private async Task ForceRelayAsync(string grantId, CancellationToken cancellation)
    {
        if (!ForceRelay || !RelayJoinerCapable) return;
        if (_legGrant != Guid.Parse(grantId))
        {
            _log("relay: FI_RELAY_FORCE set, but the grant names no usable relay; playing direct");
            return;
        }
        _log("relay: FI_RELAY_FORCE: the seat goes straight to its relay");
        await RelayFallbackAsync(new Frame.RelayFallback(Guid.Parse(grantId), RelayFallbackState.DirectFailed), cancellation).ConfigureAwait(false);
    }

    private readonly WorldOperationRetry _fallbackRetry = new(TimeProvider.System);
    private readonly ConcurrentDictionary<string, string> _relayNames = new(StringComparer.Ordinal);
    private int _fallbackRunning;
    private string? _fallbackGrant;
    // the grant the hook last reported direct for (no relay push until a new direct failure), and one whose fallback renew was refused
    private string? _directGrant;
    private string? _refusedGrant;
    // the last leg pushed (under _proofWrites): its grant, its relay's key, and whether it goes through the relay now
    private Guid? _legGrant;
    private string? _legRelayKey;
    private bool _legViaRelay;
    private string? _hostedTransport;
    private string? _hostedRelayName;

    private async Task<RelayReport?> RelayReportAsync(CancellationToken cancellation)
    {
        if (RelayReports is null) return null;
        try
        {
            var report = await RelayReports(cancellation).WaitAsync(RelayReportBound, cancellation).ConfigureAwait(false);
            foreach (var (key, name) in report.Names) _relayNames[key] = name;
            return report;
        }
        catch (Exception error) when (error is TimeoutException or IOException or HttpRequestException or InvalidOperationException
            || error is OperationCanceledException && !cancellation.IsCancellationRequested)
        {
            _log("worlds: relay report unavailable (" + error.GetType().Name + ")");
            return null;
        }
    }

    /// <summary>
    /// The leg of a verified grant, pushed right after its context (caller holds <c>_proofWrites</c>). No relay claim, or one
    /// whose address is not an IPv4 literal: no leg (a previous one is cleared). A failed write never costs the seat.
    /// </summary>
    private async Task PushLegAsync(RelayGrant relay, RelayGrantClaims verified, string? transport, CancellationToken cancellation)
    {
        if (!RelayJoinerCapable || SendRelayLeg is null) return;
        var grant = Guid.Parse(verified.Grant);
        if (verified.Relay is not { } named || !IPAddress.TryParse(named.Addr, out var address)
            || address.AddressFamily != System.Net.Sockets.AddressFamily.InterNetwork || named.Addr.Count(c => c == '.') != 3
            || address.GetAddressBytes()[0] is 0 or >= 224 || named.Port == 0
            || Volatile.Read(ref _refusedGrant) == verified.Grant) // the server refused this grant's relay: nothing for the hook to try
        {
            await ClearLegAsync(cancellation).ConfigureAwait(false);
            return;
        }
        // the hook's own report wins: once it plays direct, a seat that still reads "relay" never sends it back to the relay;
        // FI_RELAY_FORCE (a test switch) sends every leg as relay-born, so the hook never tries the direct path
        var viaRelay = ForceRelay || (Volatile.Read(ref _directGrant) != verified.Grant
            && (transport == Transports.Relay || Volatile.Read(ref _fallbackGrant) == verified.Grant));
        byte[]? claims = null, signature = null;
        Frame.RelayLeg? leg = null;
        try
        {
            claims = Convert.FromBase64String(relay.Claims);
            signature = Convert.FromBase64String(relay.Signature);
            leg = Frame.RelayLeg.Build(grant, viaRelay, address, named.Port, claims, signature);
            await SendRelayLeg(leg, cancellation).ConfigureAwait(false);
            if (_legGrant != grant || _legViaRelay != viaRelay) _log($"worlds: relay leg pushed ({(viaRelay ? "relay" : "direct")})");
            (_legGrant, _legRelayKey, _legViaRelay) = (grant, named.Key, viaRelay);
        }
        catch (Exception error) when (error is IOException or InvalidOperationException or ProtocolException or FormatException or ObjectDisposedException)
        { _log("worlds: relay leg unavailable (" + error.GetType().Name + ")"); }
        finally
        {
            if (claims is not null) CryptographicOperations.ZeroMemory(claims);
            if (signature is not null) CryptographicOperations.ZeroMemory(signature);
            if (leg is not null) CryptographicOperations.ZeroMemory(leg.Payload);
        }
    }

    /// <summary>Clears the pushed leg (caller holds <c>_proofWrites</c>).</summary>
    private async Task ClearLegAsync(CancellationToken cancellation)
    {
        if (_legGrant is null) return;
        (_legGrant, _legRelayKey, _legViaRelay) = (null, null, false);
        if (SendRelayLeg is null) return;
        try { await SendRelayLeg(Frame.RelayLeg.Clear(), cancellation).ConfigureAwait(false); }
        catch (Exception error) when (error is IOException or InvalidOperationException or ObjectDisposedException) { }
    }

    /// <summary>
    /// The hook's fallback report. <see cref="RelayFallbackState.DirectFailed"/>: renew the seat with <c>transport: "relay"</c> at once
    /// (idempotent; one at a time, repeats coalesce), then re-push the context and the leg (transport relay). Returns a toast text, or null.
    /// </summary>
    public async Task<string?> RelayFallbackAsync(Frame.RelayFallback frame, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(frame);
        if (!RelayJoinerCapable || !OnlineOpen) return null;
        var grantId = frame.Grant.ToString("D");
        if (_seat.GrantId != grantId) return null;
        var refused = Volatile.Read(ref _refusedGrant) == grantId;
        switch (frame.State)
        {
            case RelayFallbackState.RelayOk:
            case RelayFallbackState.DirectOk:
                var direct = frame.State == RelayFallbackState.DirectOk;
                await _proofWrites.WaitAsync(cancellation).ConfigureAwait(false);
                try
                {
                    if (direct)
                    {
                        // later re-pushes carry transport 0 until the hook reports a new direct failure
                        Volatile.Write(ref _directGrant, grantId);
                        Forget(ref _fallbackGrant, grantId);
                    }
                    else Forget(ref _directGrant, grantId);
                    if (_legGrant == frame.Grant) _legViaRelay = !direct;
                }
                finally { _proofWrites.Release(); }
                _log(direct ? "worlds: the seat plays direct" : "worlds: the seat plays through its relay");
                return null;
            case RelayFallbackState.RelayUnavailable:
                // a refused fallback already said why (no_relay, relay_busy): one message per grant
                if (refused) return null;
                _log("worlds: relay fallback failed; the host could not be reached");
                return _text("worlds.host_unreachable");
        }
        if (refused) return null; // the server refused this grant's relay; the hook's leg was cleared
        Forget(ref _directGrant, grantId); // a new direct failure re-arms the relay
        Volatile.Write(ref _fallbackGrant, grantId);
        if (Interlocked.CompareExchange(ref _fallbackRunning, 1, 0) != 0) return null;
        try
        {
            using var online = OnlineCancellation(cancellation);
            var token = online.Token;
            var epoch = Volatile.Read(ref _mineEpoch);
            var renewed = await OperationAsync(_fallbackRetry, t => _mm.RenewSeatAsync(grantId, Transports.Relay, t), token).ConfigureAwait(false);
            if (renewed is null || _seat.GrantId != grantId) return null;
            if (!renewed.Ok || renewed.Value is not { } answer)
            {
                _log($"worlds: relay fallback renew refused: {renewed.Error?.Kind} {renewed.Error?.Reason}");
                if (ConflictReasons.RelayKey(renewed.Error) is not { } key) return null;
                // final for this grant: the toast once, and the hook's leg cleared so it stops trying the relay
                if (Interlocked.Exchange(ref _refusedGrant, grantId) == grantId) return null;
                Forget(ref _fallbackGrant, grantId);
                await _proofWrites.WaitAsync(CancellationToken.None).ConfigureAwait(false);
                try { if (_legGrant == frame.Grant) await ClearLegAsync(CancellationToken.None).ConfigureAwait(false); }
                finally { _proofWrites.Release(); }
                return _text(key);
            }
            if (answer.Seat?.GrantId is { } returned && returned != grantId) { RequestReconcile(); return null; }
            if (!ClaimSeatProof(answer, ref epoch, grantId) || !await SendSeatProofAsync(answer.Seat, epoch, token).ConfigureAwait(false)) return null;
            lock (_mineState)
            {
                if (OnlineOpen && epoch == Volatile.Read(ref _mineEpoch) && _seat.GrantId == grantId)
                {
                    _seat.Observe(answer, Now);
                    Interlocked.Increment(ref _mineEpoch);
                }
            }
            _log("worlds: seat moved to its relay");
            return null;
        }
        catch (Exception error) when (HostFailure(error)) { return null; }
        finally { Volatile.Write(ref _fallbackRunning, 0); }
    }

    /// <summary>"Direct" / "via &lt;relay name&gt;" for the connection details; null when this session has no seat or world.</summary>
    private string? TransportText()
    {
        if (HostedWorldId is not null)
            return Volatile.Read(ref _hostedTransport) == Transports.Relay ? Via(Volatile.Read(ref _hostedRelayName)) : _text("connection.direct");
        if (_seat.GrantId is null || _legGrant is null) return null;
        return _legViaRelay ? Via(_legRelayKey is { } key && _relayNames.TryGetValue(key, out var name) ? name : null) : _text("connection.direct");
    }

    private string Via(string? name) => _text("connection.via_relay").Replace("{relay}", DisplayName(name) ?? _text("connection.relay_unnamed"), StringComparison.Ordinal);

    private static string? DisplayName(string? name)
    {
        var kept = new string((name ?? "").Where(c => !char.IsControl(c) && !char.IsSurrogate(c)).ToArray()).Trim();
        return kept.Length == 0 ? null : kept.Length > 48 ? kept[..48] : kept;
    }

    /// <summary>Clears <paramref name="slot"/> when it names <paramref name="grantId"/> (by value: grant ids are separate string instances).</summary>
    private static void Forget(ref string? slot, string grantId)
    {
        if (string.Equals(Volatile.Read(ref slot), grantId, StringComparison.Ordinal)) Volatile.Write(ref slot, null);
    }

    private void RememberRelayName(WorldRelay? relay)
    {
        if (relay is { PublicKey.Length: > 0, Name.Length: > 0 }) _relayNames[relay.PublicKey] = relay.Name;
    }
}
