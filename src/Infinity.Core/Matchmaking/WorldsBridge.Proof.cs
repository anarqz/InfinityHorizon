// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Security.Cryptography;
using System.Text.Json;
using Infinity.Core.Session;
using Infinity.Core.Official;

namespace Infinity.Core.Matchmaking;

public sealed partial class WorldsBridge
{
    public RelayGrantVerifyKeys? SocketProofKeys {get;init;}
    public Func<DateTimeOffset?>? TrustedServerNow {get;init;}
    public bool SocketProofRequired {get;init;}
    public Func<Frame.RelayContext,CancellationToken,Task>? SendRelayContext {get;set;}
    private bool NeedsProof=>BuildProfile.ProductionOnly || SocketProofRequired;
    // A development guest takes the production path whenever its grant carries proof material it can
    // verify and the hook pipe is there: context, network hint, punch, proof. requireSocketProof stays
    // the host's policy; without proof material a development grant keeps working as before.
    private bool ProofFor(RelayGrant? relay)=>NeedsProof || relay?.UdpProof is not null && SocketProofKeys is not null && SendRelayContext is not null;
    private bool ProofInstalled=>Volatile.Read(ref _proofPayload) is not null;
    private readonly SemaphoreSlim _proofWrites=new(1,1);
    private byte[]? _proofPayload;
    // Seat-view refreshes are not credential changes. Guard network replies with this generation.
    private long _proofGeneration;
    private bool ClaimSeatProof(SeatAnswer answer,ref long epoch,string? expectedGrant=null)
    {
        if(!NeedsProof)return true;
        lock(_mineState)
        {
            if(!OnlineOpen || epoch!=Volatile.Read(ref _mineEpoch)
                || !DateTimeOffset.TryParse(answer.GeneratedAt,System.Globalization.CultureInfo.InvariantCulture,System.Globalization.DateTimeStyles.AssumeUniversal,out var generated)
                || _lastSeatAt is {} previous && generated<previous
                || answer.Seat is {} seat && (seat.World.Id!=seat.WorldId || seat.State is not (SeatStates.Hosting or SeatStates.Reserved or SeatStates.Seated)
                    || expectedGrant is not null && seat.GrantId!=expectedGrant))return false;
            epoch=Interlocked.Increment(ref _mineEpoch);return true;
        }
    }

    private Frame.RelayContext ProofFrame(RelayGrant relay,string world,string grant,out RelayGrantClaims verified)
    {
        byte[]? key=null;
        try
        {
            key=relay.UdpProof?.Decode();
            if(key is null || SocketProofKeys is null || _ownXuid is null || relay.Claims is null || relay.Claims.Length>2048)throw new InvalidDataException();
            using var parsed=JsonDocument.Parse(Convert.FromBase64String(relay.Claims));
            // WORLDS_OFFICIAL §3: the host is a player's xuid or an official Worlds host's vmId (the verifier refuses both or neither)
            var hostClaim=parsed.RootElement.GetProperty("host");
            var official=hostClaim.TryGetProperty("official",out var vm) ? vm.GetString() ?? throw new InvalidDataException() : null;
            var host=official is null ? hostClaim.GetProperty("xuid").GetString() ?? throw new InvalidDataException() : null;
            verified=RelayGrantVerifier.Verify(relay,SocketProofKeys,Now.ToUnixTimeSeconds(),new(world,host,_ownXuid,"joiner",grant,HostOfficial:official));
            // the FIUP host field: the host's XUID, or the official host tag (nonzero, top bit set)
            var header=UdpProofCodec.Header(Guid.Parse(verified.Grant),Guid.Parse(verified.World),OfficialHost.HostField(verified.Host),
                ulong.Parse(verified.Xuid,System.Globalization.CultureInfo.InvariantCulture));
            var payload=new byte[89];payload[0]=1;header.AsSpan(8,48).CopyTo(payload.AsSpan(1));
            BinaryPrimitives.WriteUInt64BigEndian(payload.AsSpan(49),(ulong)verified.Exp);key.CopyTo(payload,57);
            return new(payload);
        }
        catch(Exception error) when(error is FormatException or InvalidDataException or JsonException or InvalidOperationException or KeyNotFoundException or RelayGrantException)
        { throw new InvalidDataException("Authenticated UDP socket context unavailable"); }
        finally { if(key is not null)CryptographicOperations.ZeroMemory(key); }
    }
    private async Task<bool> SendProofAsync(RelayGrant relay,string world,string grant,long epoch,CancellationToken cancellation,string? transport=null)
    {
        ObserveSeatTransport(grant,transport); // OD-160's notice: the seat's transport as the server gave it
        if(!ProofFor(relay))return true;
        Frame.RelayContext frame;
        try { frame=ProofFrame(relay,world,grant,out _); }
        catch(InvalidDataException)
        {
            await ClearProofAsync("the seat's grant proof is unusable",CancellationToken.None,epoch).ConfigureAwait(false);
            if(NeedsProof)return false;
            _log("worlds: development grant proof unusable; joining without a socket proof");
            return true; // development only: never worse than the old path
        }
        var entered=false;
        try
        {
            await _proofWrites.WaitAsync(cancellation).ConfigureAwait(false);entered=true;
            if(!OnlineOpen || epoch!=Volatile.Read(ref _mineEpoch) || SendRelayContext is null)return false;
            try { await SendRelayContext(frame,cancellation).ConfigureAwait(false); }
            catch(Exception error) when(!NeedsProof && error is not OperationCanceledException)
            {
                // development only: a context the hook cannot take (pipe not ready, no clock) never costs the seat
                _log("worlds: development guest could not push the socket proof context; joining without a socket proof");
                return true;
            }
            // T35: a newer seat read, renewal or reconcile (the epoch moved while the context was written) does not cost a seat that
            // still holds this grant its context: it is kept and the newer view decides. Only online
            // closing or the seat ending meanwhile clears them, with its reason in the log.
            var stale=epoch!=Volatile.Read(ref _mineEpoch);
            if(!OnlineOpen || stale && _seat.GrantId!=grant)
            {
                await SendRelayContext(new([0]),CancellationToken.None).ConfigureAwait(false);
                if(_proofPayload is not null) {CryptographicOperations.ZeroMemory(_proofPayload);_proofPayload=null;_proofGeneration++;}
                _log("worlds: seat context cleared (" + (!OnlineOpen ? "online closed while the seat's context was written" : "the seat ended or changed while its context was written") + ")");
                return false;
            }
            if(stale)_log("worlds: a newer seat view arrived while the context was written; the same grant keeps its context");
            if(_proofPayload is null || !_proofPayload.AsSpan(1,48).SequenceEqual(frame.Payload.AsSpan(1,48))
                || !CryptographicOperations.FixedTimeEquals(_proofPayload.AsSpan(57,32),frame.Payload.AsSpan(57,32)))
            { _networkRevision=0;_proofGeneration++; }
            if(_proofPayload is not null)CryptographicOperations.ZeroMemory(_proofPayload);
            _proofPayload=frame.Payload.ToArray();
            if(!NeedsProof)_log("worlds: development guest installed the socket proof context");
            return true;
        }
        finally {CryptographicOperations.ZeroMemory(frame.Payload);if(entered)_proofWrites.Release();}
    }
    private Task<bool> SendSeatProofAsync(Seat? seat,long epoch,CancellationToken cancellation)
    {
        if(!NeedsProof && !ProofFor(seat?.Relay))return ProofInstalled ? ClearSeatProofAsync("the seat's grant carries no proof",epoch) : Task.FromResult(true);
        // T35: the server's answer shows no seat (it ended: released, expired, kicked, its world closed): the hook stops using it
        if(seat is null)return ClearSeatProofAsync("the server shows no seat",epoch);
        if(seat.State==SeatStates.Hosting)return ClearSeatProofAsync("this player hosts its world",epoch);
        if(seat.Relay is null || seat.GrantId is null)return ClearSeatProofAsync("the seat has no grant",epoch,success:false);
        return SendProofAsync(seat.Relay,seat.WorldId,seat.GrantId,epoch,cancellation,seat.Transport);
    }
    private async Task<bool> ClearSeatProofAsync(string why,long epoch,bool success=true) {await ClearProofAsync(why,CancellationToken.None,epoch).ConfigureAwait(false);return success;}
    /// <summary>Clears the hook's socket context; <paramref name="why"/> goes to the log line (T35).</summary>
    private async Task ClearProofAsync(string why,CancellationToken cancellation,long? expectedEpoch=null)
    {
        if(!NeedsProof && !ProofInstalled)return; // a development context, once pushed, is cleared like a production one
        var epoch=expectedEpoch ?? Volatile.Read(ref _mineEpoch);
        using var bound=CancellationTokenSource.CreateLinkedTokenSource(cancellation);bound.CancelAfter(TimeSpan.FromSeconds(2));
        await _proofWrites.WaitAsync(bound.Token).ConfigureAwait(false);
        try
        {
            if(epoch!=Volatile.Read(ref _mineEpoch))return;
            var installed=_proofPayload is not null;
            if(_proofPayload is not null) {CryptographicOperations.ZeroMemory(_proofPayload);_proofPayload=null;_proofGeneration++;}
            _networkRevision=0;
            if(SendRelayContext is not null)await SendRelayContext(new([0]),bound.Token).ConfigureAwait(false);
            if(installed)_log("worlds: seat context cleared ("+why+")");
        }
        finally {_proofWrites.Release();}
    }
}
