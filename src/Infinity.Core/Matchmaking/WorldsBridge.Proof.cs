// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Security.Cryptography;
using System.Text.Json;
using Infinity.Core.Session;
using Infinity.Core.WorldServer;

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
            var host=parsed.RootElement.GetProperty("host").GetProperty("xuid").GetString() ?? throw new InvalidDataException();
            verified=RelayGrantVerifier.Verify(relay,SocketProofKeys,Now.ToUnixTimeSeconds(),new(world,host,_ownXuid,"joiner",grant));
            var header=UdpProofCodec.Header(Guid.Parse(verified.Grant),Guid.Parse(verified.World),ulong.Parse(verified.Host.Xuid,System.Globalization.CultureInfo.InvariantCulture),
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
        RelayGrantClaims verified;
        try { frame=ProofFrame(relay,world,grant,out verified); }
        catch(InvalidDataException)
        {
            await ClearProofAsync(CancellationToken.None,epoch).ConfigureAwait(false);
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
            if(!OnlineOpen || epoch!=Volatile.Read(ref _mineEpoch))
            {
                await SendRelayContext(new([0]),CancellationToken.None).ConfigureAwait(false);
                await ClearLegAsync(CancellationToken.None).ConfigureAwait(false);return false;
            }
            if(_proofPayload is null || !_proofPayload.AsSpan(1,48).SequenceEqual(frame.Payload.AsSpan(1,48))
                || !CryptographicOperations.FixedTimeEquals(_proofPayload.AsSpan(57,32),frame.Payload.AsSpan(57,32)))
            { _networkRevision=0;_proofGeneration++; }
            if(_proofPayload is not null)CryptographicOperations.ZeroMemory(_proofPayload);
            _proofPayload=frame.Payload.ToArray();
            await PushLegAsync(relay,verified,transport,cancellation).ConfigureAwait(false); // CR-S57: beside every context push
            if(!NeedsProof)_log("worlds: development guest installed the socket proof context");
            return true;
        }
        finally {CryptographicOperations.ZeroMemory(frame.Payload);if(entered)_proofWrites.Release();}
    }
    private Task<bool> SendSeatProofAsync(Seat? seat,long epoch,CancellationToken cancellation)
    {
        if(!NeedsProof && !ProofFor(seat?.Relay))return ProofInstalled ? ClearSeatProofAsync(epoch) : Task.FromResult(true);
        if(seat is null || seat.State==SeatStates.Hosting)return ClearSeatProofAsync(epoch);
        if(seat.Relay is null || seat.GrantId is null)return ClearSeatProofAsync(epoch,success:false);
        RememberRelayName(seat.World.Relay);
        return SendProofAsync(seat.Relay,seat.WorldId,seat.GrantId,epoch,cancellation,seat.Transport);
    }
    private async Task<bool> ClearSeatProofAsync(long epoch,bool success=true) {await ClearProofAsync(CancellationToken.None,epoch).ConfigureAwait(false);return success;}
    private async Task ClearProofAsync(CancellationToken cancellation,long? expectedEpoch=null)
    {
        if(!NeedsProof && !ProofInstalled)return; // a development context, once pushed, is cleared like a production one
        var epoch=expectedEpoch ?? Volatile.Read(ref _mineEpoch);
        using var bound=CancellationTokenSource.CreateLinkedTokenSource(cancellation);bound.CancelAfter(TimeSpan.FromSeconds(2));
        await _proofWrites.WaitAsync(bound.Token).ConfigureAwait(false);
        try
        {
            if(epoch!=Volatile.Read(ref _mineEpoch))return;
            if(_proofPayload is not null) {CryptographicOperations.ZeroMemory(_proofPayload);_proofPayload=null;_proofGeneration++;}
            _networkRevision=0;
            if(SendRelayContext is not null)await SendRelayContext(new([0]),bound.Token).ConfigureAwait(false);
            await ClearLegAsync(bound.Token).ConfigureAwait(false);
        }
        finally {_proofWrites.Release();}
    }
}
