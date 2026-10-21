// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Threading.Channels;
using Infinity.Core.Session;

namespace Infinity.Core.Diagnostics;

/// <summary>Scalar allowlist, bounded attempts/rate/queue. No networking or file I/O on a UDP actor.</summary>
public sealed class UdpDiagnostics(Action<string> log, TimeProvider? clock = null)
{
    private sealed class Attempt { internal long At; internal bool Seen,Final; }
    private readonly TimeProvider _clock=clock??TimeProvider.System;
    private readonly Dictionary<Guid,Attempt> _attempts=[];
    private readonly object _gate=new();
    private int _lines;
    private static readonly string[] Phases=["", "context_ready","context_cleared","clock_ready","clock_missing","socket_bound",
        "socket_bind_failed","stun_sent","stun_timeout","mapping_ready","mapping_failed","hint_published","hint_failed",
        "candidates_received","candidates_exhausted","hello_sent","challenge_received","response_sent","ack_accepted",
        "proof_rejected","proof_timeout","route_selected","control_closed","summary","hello_received","challenge_sent",
        "response_received","ack_sent","stun_received"];
    private static readonly Channel<(Action<string> Sink,string Line)> Queue=Channel.CreateBounded<(Action<string>,string)>(
        new BoundedChannelOptions(64){SingleReader=true,FullMode=BoundedChannelFullMode.Wait});
    private static int _dropped;
    public static int Dropped=>Volatile.Read(ref _dropped);
    private static readonly Task Writer=Task.Run(async()=>
    {
        await foreach(var item in Queue.Reader.ReadAllAsync().ConfigureAwait(false))
            try {item.Sink(item.Line);} catch(Exception) { Drop(); }
    });
    public void Native(Frame.UdpDiagnostic frame)
    {
        var p=frame.Payload;if(!Frame.UdpDiagnostic.Valid(p))return;
        var id=new Guid(p.AsSpan(4,16));var final=p[1]==23;
        lock(_gate)
        {
            if(!_attempts.TryGetValue(id,out var attempt))
            {if(_attempts.Count==64)return;_attempts.Add(id,attempt=new());}
            if(attempt.Final || attempt.Seen && !final && _clock.GetElapsedTime(attempt.At)<TimeSpan.FromSeconds(1) || _lines>=2048)return;
            attempt.At=_clock.GetTimestamp();attempt.Seen=true;attempt.Final=final;_lines++;
            Enqueue(log,FormattableString.Invariant($"udp native attempt={id:N} phase={Phases[p[1]]} kind={p[2]} candidate={p[3]} elapsed_ms={BinaryPrimitives.ReadUInt32BigEndian(p.AsSpan(20))} tx={BinaryPrimitives.ReadUInt32BigEndian(p.AsSpan(24))} rx={BinaryPrimitives.ReadUInt32BigEndian(p.AsSpan(28))}"));
        }
    }
    public enum ManagedPhase { HintStarted,HintAccepted,HintRefused,HintExpired,HintStale,HintFailed,HostSnapshot,JoinHostBlocked,JoinSeatPreserved }
    private readonly long[] _managedAt=new long[9];
    private readonly bool[] _managedSeen=new bool[9];
    public void Managed(ManagedPhase phase,uint revision=0,int count=0,int ttl=0)
    {
        if(!Enum.IsDefined(phase))return;
        lock(_gate)
        {
            var i=(int)phase;
            if(_lines>=2048 || _managedSeen[i] && _clock.GetElapsedTime(_managedAt[i])<TimeSpan.FromSeconds(1))return;
            _managedAt[i]=_clock.GetTimestamp();_managedSeen[i]=true;_lines++;
            Enqueue(log,FormattableString.Invariant($"udp managed phase={phase} revision={revision} count={Math.Clamp(count,0,64)} ttl={Math.Clamp(ttl,0,45)}"));
        }
    }
    private static void Drop()
    {
        int old;
        do {old=Volatile.Read(ref _dropped);if(old==int.MaxValue)return;}
        while(Interlocked.CompareExchange(ref _dropped,old+1,old)!=old);
    }
    internal static void Enqueue(Action<string> sink,string line) { _=Writer;if(!Queue.Writer.TryWrite((sink,line)))Drop(); }
}
