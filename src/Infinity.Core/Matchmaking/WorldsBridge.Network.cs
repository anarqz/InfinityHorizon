// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using Infinity.Core.Session;
using Infinity.Core.Wire;
using static Infinity.Core.Diagnostics.UdpDiagnostics;

namespace Infinity.Core.Matchmaking;

public sealed partial class WorldsBridge
{
    public Func<Frame.RelayCandidates,CancellationToken,Task>? SendRelayCandidates {get;set;}
    private uint _networkRevision;
    private int _networkRunning;

    /// <summary>One bounded request off the supervisor loop; no address or credential logging.</summary>
    public async Task NetworkHintAsync(Frame.RelayNetworkHint hint,CancellationToken cancellation)
    {
        if(!NeedsProof && !ProofInstalled || Interlocked.CompareExchange(ref _networkRunning,1,0)!=0)return;
        using var bound=CancellationTokenSource.CreateLinkedTokenSource(cancellation,_onlineStop.Token);
        bound.CancelAfter(TimeSpan.FromSeconds(8));
        var started=_clock.GetTimestamp();
        try
        {
            RelayNetworkFrames.Hint(hint.Payload);
            var grant=new Guid(hint.Payload.AsSpan(0,16),true);
            var revision=BinaryPrimitives.ReadUInt32BigEndian(hint.Payload.AsSpan(16));
            string world;
            long proofGeneration;
            await _proofWrites.WaitAsync(bound.Token).ConfigureAwait(false);
            try
            {
                if(!OnlineOpen || _proofPayload is not {Length:89} context || new Guid(context.AsSpan(1,16),true)!=grant
                    || revision<=_networkRevision || SendRelayCandidates is null)return;
                _networkRevision=revision;proofGeneration=_proofGeneration;world=new Guid(context.AsSpan(17,16),true).ToString("D");
                _udpDiagnostics.Managed(ManagedPhase.HintStarted,revision);
            }
            finally {_proofWrites.Release();}
            var p=hint.Payload;
            var request=new SeatNetworkRequest{GrantId=grant.ToString("D"),Revision=revision,Endpoint=new(){
                Port=BinaryPrimitives.ReadUInt16BigEndian(p.AsSpan(20)),Lan=_lan().Take(4).ToArray(),
                External=p[22]==0?null:new(){Port=BinaryPrimitives.ReadUInt16BigEndian(p.AsSpan(23)),Source=p[22]==1?"stun":"upnp"}}};
            var answer=await _mm.SeatNetworkAsync(request,bound.Token).WaitAsync(bound.Token).ConfigureAwait(false);
            if(answer is not {Ok:true,Value:{} value} || value.GrantId!=request.GrantId || value.WorldId!=world || value.Revision!=revision
                || !DateTimeOffset.TryParse(value.GeneratedAt,System.Globalization.CultureInfo.InvariantCulture,System.Globalization.DateTimeStyles.AssumeUniversal,out var generated)
                || !DateTimeOffset.TryParse(value.ExpiresAt,System.Globalization.CultureInfo.InvariantCulture,System.Globalization.DateTimeStyles.AssumeUniversal,out var expires))
            {_udpDiagnostics.Managed(ManagedPhase.HintRefused,revision);return;}
            var ttl=(int)Math.Floor(Math.Min(30,(expires-generated).TotalSeconds)-_clock.GetElapsedTime(started).TotalSeconds);
            if(TrustedServerNow?.Invoke() is not {} serverNow || expires<=serverNow || generated>serverNow+TimeSpan.FromSeconds(1))
            {_udpDiagnostics.Managed(ManagedPhase.HintExpired,revision);return;}
            ttl=Math.Min(ttl,(int)Math.Floor((expires-serverNow).TotalSeconds));
            if(ttl<1) {_udpDiagnostics.Managed(ManagedPhase.HintExpired,revision);return;}
            var frame=new Frame.RelayCandidates(RelayNetworkFrames.Candidates(grant,revision,(byte)ttl,value.Candidates));
            await _proofWrites.WaitAsync(bound.Token).ConfigureAwait(false);
            try
            {
                if(!OnlineOpen || proofGeneration!=_proofGeneration || revision!=_networkRevision || SendRelayCandidates is null
                    || _proofPayload is not {Length:89} context || new Guid(context.AsSpan(1,16),true)!=grant)
                {_udpDiagnostics.Managed(ManagedPhase.HintStale,revision);return;}
                await SendRelayCandidates(frame,bound.Token).ConfigureAwait(false);
                _udpDiagnostics.Managed(ManagedPhase.HintAccepted,revision,value.Candidates.Count,ttl);
            }
            finally {_proofWrites.Release();}
        }
        catch(Exception error) when(error is IOException or OperationCanceledException or HttpRequestException or InvalidOperationException
            or TimeoutException or ProtocolException) {_udpDiagnostics.Managed(ManagedPhase.HintFailed);}
        finally {Volatile.Write(ref _networkRunning,0);}
    }
}
