// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Api;

/// <summary>
/// OD-118: a request to an Infinity host (API, gate, saves, matchmaking, the IdP behind sign-in) while the launcher is offline: from
/// the start until the player clicks SIGN IN (or resumes a session they chose online), and from Play offline on. Derives from
/// <see cref="HttpRequestException"/>, so every caller already fails closed on it, as on a network that cannot be reached.
/// </summary>
public sealed class OfflineModeException(string host) : HttpRequestException("offline: no request to " + host + " before Sign in")
{
    public string Host { get; } = host;
}

/// <summary>
/// The one handler in front of every Infinity client (OD-118). Each request is counted, then refused with
/// <see cref="OfflineModeException"/> unless <c>allowed()</c>; nothing is resolved or connected for a refused request. A refusal is
/// a bug in the launcher's gating: it is logged in every build, and a development build also reports it through
/// <see cref="Violation"/> so tests and the developer see it.
/// </summary>
public sealed class InfinityRequestGate(Func<bool> allowed, Action<string> log, bool development, HttpMessageHandler inner)
    : DelegatingHandler(inner)
{
    /// <summary>How many requests <see cref="Seen"/> keeps (the newest): a launcher left open for days polls without end.</summary>
    public const int MaxSeen = 4096;

    private readonly object _lock = new();
    private readonly Queue<string> _seen = new();

    /// <summary>Raised (development builds) with the refused request's method and host.</summary>
    public event Action<string>? Violation;

    /// <summary>The newest <see cref="MaxSeen"/> requests that reached this gate, refused or not, oldest first: "METHOD host/path".</summary>
    public IReadOnlyList<string> Seen { get { lock (_lock) return [.. _seen]; } }

    /// <summary>Every request that reached this gate since the start.</summary>
    public long Requests { get; private set; }

    /// <summary>Tests only: answers allowed requests instead of the transport, so a test never reaches a real host.</summary>
    public Func<HttpRequestMessage, HttpResponseMessage>? Stub { get; set; }

    /// <summary>How many were refused.</summary>
    public int Refused { get; private set; }

    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        var host = request.RequestUri?.Host ?? "?";
        lock (_lock)
        {
            Requests++;
            if (_seen.Count == MaxSeen) _seen.Dequeue();
            _seen.Enqueue(request.Method.Method + " " + host + request.RequestUri?.AbsolutePath);
        }
        if (allowed()) return Stub is { } stub ? Task.FromResult(stub(request)) : base.SendAsync(request, cancellationToken);
        lock (_lock) Refused++;
        var what = request.Method.Method + " " + host;
        log((development ? "OFFLINE GUARD VIOLATION: " : "offline: ") + what + " refused (no Infinity request before Sign in)");
        if (development) Violation?.Invoke(what);
        throw new OfflineModeException(host);
    }
}
