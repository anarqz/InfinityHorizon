// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;
using Infinity.Core.Online;
using Infinity.Core.WorldServer;

namespace Infinity.Core.Matchmaking;

/// <summary>Production adapter: the bound transport and verified world admission share one lifetime.</summary>
public sealed class IntegratedWorldRelay : IHostRelay, IAsyncDisposable
{
    private readonly IntegratedHostRelay _transport;
    private readonly WorldRelayAdmission _admission;
    private int _disposed;
    public Action<string>? DiagnosticLog {get;set;}
    private long _diagnosticAt;
    private int _diagnosticCount;
    private void Diagnostics(bool final=false)
    {
        var now=Environment.TickCount64;
        if(DiagnosticLog is not {} sink || !final && (Volatile.Read(ref _diagnosticCount)>=2048 || now-Interlocked.Read(ref _diagnosticAt)<3000))return;
        Interlocked.Exchange(ref _diagnosticAt,now);
        Interlocked.Increment(ref _diagnosticCount);
        var network=NetworkStatus;
        Infinity.Core.Diagnostics.UdpDiagnostics.Enqueue(sink,_admission.DiagnosticSummary()+
            FormattableString.Invariant($" nat={network.Nat} observations={network.PublicAddresses.Count} upnp={network.Upnp} punch_hints={network.PunchHints} punch_sent={network.PunchesSent} send_errors={network.SendErrors}"));
    }

    private IntegratedWorldRelay(IntegratedHostRelay transport, WorldRelayAdmission admission)
    { _transport = transport; _admission = admission; }

    public ushort Port => _transport.Port;
    public bool Running => Volatile.Read(ref _disposed) == 0 && _transport.Running;
    public RelayNetworkStatus NetworkStatus => _transport.NetworkStatus;
    public Diagnostics.CrashContext? CrashContext { get => _transport.CrashContext; set => _transport.CrashContext = value; }

    public static IntegratedWorldRelay Start(uint maxPlayers, bool races, string ownXuid, RelayGrantVerifyKeys keys,
        ushort port = HostRelay.DefaultPort, IntegratedRelayOptions? options = null, TimeProvider? clock = null, bool devLanPeers = false, bool requireSocketProof=false)
    {
        ArgumentNullException.ThrowIfNull(keys);
        if (keys.Count == 0 || !ulong.TryParse(ownXuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid) || xuid == 0)
            throw new InvalidOperationException("Integrated relay trust is not configured");
        return new(IntegratedHostRelay.Start(maxPlayers, races, port, options), new(xuid, keys, clock, devLanPeers,requireSocketProof));
    }

    /// <summary>Only a successful authenticated PUT/announce may refresh the 45-second world lease.</summary>
    public async Task UpdateWorldAsync(WorldAnnounced world, CancellationToken cancellation = default)
    {
        ObjectDisposedException.ThrowIf(!Running, this);
        cancellation.ThrowIfCancellationRequested();
        try
        {
            _admission.Update(world);
            await _transport.SetAdmissionPolicyAsync(_admission.Allowed,_admission.AllowedHook,
                (packet,endpoint)=>_admission.ReceiveProof(packet.Span,endpoint),cancellation).ConfigureAwait(false);
            _transport.PunchNetworkHints(_admission);
            _transport.SetRelayLegs(_admission.RelayLegs());
        }
        catch { EndAdmission(); throw; }
    }

    /// <summary>No admission, and no relay leg either: nothing more is said to a relay for a grant this world no longer holds.</summary>
    private void EndAdmission() { _admission.Clear(); _transport.SetRelayLegs([]); }

    public Task<IReadOnlyList<string>> PeersAsync(CancellationToken cancellation) => _transport.PeersAsync(cancellation);

    /// <summary>Fresh grants/revocations every three seconds; this read never renews the world's host lease.</summary>
    public async Task RefreshAdmissionsAsync(IMatchmakingApi matchmaking, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(matchmaking);
        ObjectDisposedException.ThrowIf(!Running, this);
        var answer = await matchmaking.PunchesAsync(cancellation).ConfigureAwait(false);
        if (answer.Error is { } error && OnlinePause.From(error.Code, answer.Status, error.Details) is not null)
        {
            EndAdmission();
            throw new ApiException(answer.Status, error.Code, "Online activity closed", error.Details, answer.RetryAfter);
        }
        cancellation.ThrowIfCancellationRequested();
        ObjectDisposedException.ThrowIf(!Running, this);
        if (answer is { Ok: true, Value: { } punches }) { _admission.Refresh(punches);_transport.PunchNetworkHints(_admission);_transport.SetRelayLegs(_admission.RelayLegs());Diagnostics(); return; }
        if (answer.Error?.Kind is MmErrorKind.Unauthorized or MmErrorKind.WorldNotFound or MmErrorKind.NotHost or MmErrorKind.WrongPool)
        {
            EndAdmission();
            throw new InvalidOperationException("Integrated world admission ended");
        }
        // Keep the verified expiring snapshot, but preserve retry metadata for the bridge's circuit.
        throw new ApiException(answer.Status, answer.Error?.Code ?? "HTTP.BAD_BODY", "Admission refresh unavailable",
            answer.Error?.Details, answer.RetryAfter ?? answer.Error?.RetryAfter);
    }

    public void Kick(string xuid)
    {
        if (ulong.TryParse(xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var id)) _admission.Revoke(id);
        _transport.Kick(xuid);
    }

    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        Diagnostics(final:true);
        EndAdmission(); _transport.Dispose();
    }

    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0) return;
        Diagnostics(final:true);
        EndAdmission();
        await Task.Run(_transport.Dispose).ConfigureAwait(false);
    }
}
