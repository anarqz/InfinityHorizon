// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net;
using System.Net.Sockets;

namespace Infinity.Core.Matchmaking;

/// <summary>CR-S40: dedicated IPv4 observation for IPv4 STUN ports; original HTTPS authority/TLS.</summary>
public static class MatchmakingTransport
{
    public static HttpClient Create()=>new(CreateHandler())
    {
        Timeout=TimeSpan.FromSeconds(30),DefaultRequestVersion=HttpVersion.Version11,
        DefaultVersionPolicy=HttpVersionPolicy.RequestVersionOrLower,
    };

    internal static SocketsHttpHandler CreateHandler(Ipv4Connection? connection=null)
    {
        connection??=new();
        return new()
        {
            UseProxy=false,AllowAutoRedirect=false,UseCookies=false,
            ConnectTimeout=TimeSpan.FromSeconds(6),MaxConnectionsPerServer=4,
            PooledConnectionLifetime=TimeSpan.FromMinutes(2),PooledConnectionIdleTimeout=TimeSpan.FromSeconds(30),
            ConnectCallback=(context,cancellation)=>connection.OpenAsync(context.DnsEndPoint,cancellation),
        };
    }
}

/// <summary>Bounded A lookup and at most four IPv4 TCP attempts. No TLS overrides or IPv6 fallback.</summary>
internal sealed class Ipv4Connection(
    Func<string,CancellationToken,Task<IPAddress[]>>? resolve=null,
    Func<Socket,IPEndPoint,CancellationToken,Task>? connect=null,
    TimeSpan? dnsBudget=null,TimeSpan? attemptBudget=null,TimeSpan? totalBudget=null)
{
    private readonly Func<string,CancellationToken,Task<IPAddress[]>> _resolve=resolve??
        ((host,cancellation)=>Dns.GetHostAddressesAsync(host,AddressFamily.InterNetwork,cancellation));
    private readonly Func<Socket,IPEndPoint,CancellationToken,Task> _connect=connect??
        ((socket,endpoint,cancellation)=>socket.ConnectAsync(endpoint,cancellation).AsTask());
    internal async ValueTask<Stream> OpenAsync(DnsEndPoint endpoint,CancellationToken cancellation)
    {
        using var total=CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        total.CancelAfter(totalBudget??TimeSpan.FromSeconds(6));
        try
        {
            IPAddress[] addresses;
            if(IPAddress.TryParse(endpoint.Host,out var literal))addresses=[literal];
            else
            {
                using var dns=CancellationTokenSource.CreateLinkedTokenSource(total.Token);
                dns.CancelAfter(dnsBudget??TimeSpan.FromSeconds(2));
                addresses=await _resolve(endpoint.Host,dns.Token).WaitAsync(dns.Token).ConfigureAwait(false);
            }
            foreach(var address in addresses.Where(a=>a.AddressFamily==AddressFamily.InterNetwork).Distinct().Take(4))
            {
                total.Token.ThrowIfCancellationRequested();
                var socket=new Socket(AddressFamily.InterNetwork,SocketType.Stream,ProtocolType.Tcp);
                var transferred=false;
                try
                {
                    socket.NoDelay=true;
                    using var attempt=CancellationTokenSource.CreateLinkedTokenSource(total.Token);
                    attempt.CancelAfter(attemptBudget??TimeSpan.FromSeconds(1));
                    var pending=_connect(socket,new(address,endpoint.Port),attempt.Token);
                    // Disposed sockets can finish failing after cancellation. Observe without logging endpoints.
                    _=pending.ContinueWith(t=>{_=t.Exception;},CancellationToken.None,
                        TaskContinuationOptions.OnlyOnFaulted|TaskContinuationOptions.ExecuteSynchronously,TaskScheduler.Default);
                    await pending.WaitAsync(attempt.Token).ConfigureAwait(false);
                    attempt.Token.ThrowIfCancellationRequested();
                    total.Token.ThrowIfCancellationRequested();
                    var stream=new NetworkStream(socket,ownsSocket:true);transferred=true;return stream;
                }
                catch(OperationCanceledException) when(!total.IsCancellationRequested) { }
                catch(SocketException) { }
                finally {if(!transferred)socket.Dispose();}
            }
        }
        catch(OperationCanceledException) when(!cancellation.IsCancellationRequested) { }
        catch(SocketException) { }
        cancellation.ThrowIfCancellationRequested();
        throw new HttpRequestException("IPv4 matchmaking connection unavailable.");
    }
}
