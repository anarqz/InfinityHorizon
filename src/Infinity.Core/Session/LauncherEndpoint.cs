// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Session;

/// <summary>
/// The game access ticket's bearer token and its expiry (ADR 0014: never the platform session).
/// Only this type fits a <see cref="Frame.Token"/>.
/// </summary>
public sealed record GameToken(Secret Value, ulong ExpiresUnix)
{
    public Frame.Token ToFrame() => new(Value, ExpiresUnix);
}

/// <summary>What the server said about this launch, handed to the hook at <c>Hello</c>.</summary>
public abstract record Admission
{
    private Admission()
    {
    }

    /// <summary>Signed claims; the token goes in at <c>Hello</c> time from the current one.</summary>
    public sealed record Ticket(byte[] Claims, byte[] Signature) : Admission;

    /// <summary>No session this time (refused, queue left, offline): the hook plays offline and says why.</summary>
    public sealed record Refused(string Reason) : Admission;
}

/// <summary>What the supervisor learns from one hook frame.</summary>
public abstract record EndpointEvent
{
    private EndpointEvent()
    {
    }

    public sealed record HookConnected(string HookVersion, uint Pid) : EndpointEvent;

    /// <summary>Heartbeat now, then push <see cref="GameToken.ToFrame"/>.</summary>
    public sealed record TokenRefreshNeeded : EndpointEvent;

    public sealed record RelaunchRequested(byte Reason) : EndpointEvent;

    public sealed record HookEvent(byte Kind, string Detail) : EndpointEvent;

    public sealed record Closed(byte Reason) : EndpointEvent;

    /// <summary>Frame 0x13 from the hook.</summary>
    public sealed record Capture(byte What, bool Ok) : EndpointEvent;

    /// <summary>Frame 0x1b from the hook (N-H2): what the native mod runtime initialized for this launch.</summary>
    public sealed record ModsReported(Frame.ModsReport Report) : EndpointEvent;

    /// <summary>Frame 0x1f from the hook (CR-L40): FREE ROAM ONLINE waits for <see cref="Frame.FreeRoamReady"/> with this nonce.</summary>
    public sealed record FreeRoamNeeded(uint Nonce) : EndpointEvent;
}

/// <summary>
/// The launcher side of the control-pipe conversation as a pure state machine (session.rs
/// <c>LauncherEndpoint</c>; the transport only moves bytes). <c>Hello</c> is answered once, with
/// the ticket and the token current at that moment.
/// </summary>
public sealed class LauncherEndpoint
{
    private readonly uint _minBuild;
    private Admission? _admission;

    /// <summary>
    /// <paramref name="minBuild"/>: a <c>Hello</c> whose hook version is below it, or carries no
    /// build, is answered with <c>Refused</c> (ORCHESTRATOR §2.1). 0 disables the check; the
    /// supervisor passes <see cref="Launch.FirstProtocolBuild"/>.
    /// </summary>
    /// <summary>The first hook build that takes <see cref="Frame.CustomTrust"/> (CR-L39): an older one would refuse a custom server's ticket.</summary>
    public const uint CustomTrustHookBuild = 89;

    private readonly Frame.CustomTrust? _customTrust;

    /// <param name="customTrust">A custom server's pinned launch-ticket keys (CR-L39), sent right before the Ticket; null for the official server.</param>
    public LauncherEndpoint(Admission admission, uint minBuild, Frame.CustomTrust? customTrust)
        : this(admission, customTrust is null ? minBuild : Math.Max(minBuild, CustomTrustHookBuild))
    {
        _customTrust = customTrust;
    }

    public LauncherEndpoint(Admission admission, uint minBuild = 0)
    {
        _admission = admission ?? throw new ArgumentNullException(nameof(admission));
        _minBuild = minBuild;
    }

    /// <summary>True once <c>Hello</c> was answered.</summary>
    public bool Greeted { get; private set; }

    public bool OnlineSuspended { get; private set; }

    /// <summary>Latch before delivery. Before Hello refuse admission; afterward return the one terminal control frame.</summary>
    public Frame.OnlineSuspend? Suspend(OnlineSuspendReason reason)
    {
        if (!Enum.IsDefined(reason)) throw new ProtocolException("unknown online suspension reason");
        if (OnlineSuspended) return null;
        OnlineSuspended = true;
        if (!Greeted)
        {
            _admission = new Admission.Refused("online_suspended");
            return null;
        }
        return new Frame.OnlineSuspend(reason);
    }

    /// <summary>The gate ended before <c>Hello</c>: the hook gets <c>Refused</c> instead. No effect once greeted.</summary>
    public void Revoke(string reason)
    {
        if (!Greeted)
        {
            _admission = new Admission.Refused(reason);
        }
    }

    /// <summary>One frame from the hook. <paramref name="current"/> is the newest game token.</summary>
    public (IReadOnlyList<Frame> Replies, EndpointEvent? Event) Handle(Frame frame, GameToken current)
    {
        ArgumentNullException.ThrowIfNull(frame);
        ArgumentNullException.ThrowIfNull(current);
        if (frame is Frame.Hello hello)
        {
            if (Greeted)
            {
                throw new ProtocolException("second Hello");
            }
            Greeted = true;
            var connected = new EndpointEvent.HookConnected(hello.HookVersion, hello.Pid);
            if (hello.Protocol != Launch.ProtocolVersion)
            {
                _admission = null;
                return ([new Frame.Refused($"hook protocol {hello.Protocol}, launcher speaks {Launch.ProtocolVersion}: update both")], connected);
            }
            if (_minBuild > 0 && (Launch.BuildNumber(hello.HookVersion) is not { } build || build < _minBuild))
            {
                _admission = null;
                return ([new Frame.Refused($"hook {hello.HookVersion} does not speak the launcher protocol (needs build {_minBuild} or newer): update ForzaInfinity")], connected);
            }
            var admission = _admission ?? throw new ProtocolException("ticket already used");
            _admission = null;
            Frame reply = admission switch
            {
                Admission.Ticket t => new Frame.Ticket(t.Claims, t.Signature, current.Value, current.ExpiresUnix),
                Admission.Refused r => new Frame.Refused(r.Reason),
                _ => throw new InvalidOperationException(),
            };
            return (reply is Frame.Ticket && _customTrust is { } trust ? [trust, reply] : [reply], connected);
        }
        if (!Greeted)
        {
            throw new ProtocolException("first frame must be Hello");
        }
        if (OnlineSuspended && frame is Frame.RefreshRequest or Frame.RelaunchRequest or Frame.Event { Kind: 3 or 5 })
            return ([], null);
        EndpointEvent evt = frame switch
        {
            Frame.RefreshRequest => new EndpointEvent.TokenRefreshNeeded(),
            Frame.RelaunchRequest r => new EndpointEvent.RelaunchRequested(r.Reason),
            Frame.Event e => new EndpointEvent.HookEvent(e.Kind, e.Detail),
            Frame.Bye b => new EndpointEvent.Closed(b.Reason),
            Frame.Capture c => new EndpointEvent.Capture(c.What, c.Ok),
            Frame.ModsReport m => new EndpointEvent.ModsReported(m),
            Frame.FreeRoamNeed f => new EndpointEvent.FreeRoamNeeded(f.Nonce),
            _ => throw new ProtocolException($"unexpected frame from hook: type 0x{frame.Type:x2}"),
        };
        return ([], evt);
    }
}
