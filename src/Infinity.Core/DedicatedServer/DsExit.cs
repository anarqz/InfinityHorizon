// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.DedicatedServer;

/// <summary>Why a worlds process ended (DEDICATED_SERVER.md §2.1); every kind means its worlds are gone.</summary>
public enum DsExitKind
{
    /// <summary>0: drain finished, <c>shutdown</c>, or the control channel closed.</summary>
    Clean,

    /// <summary>2: usage or configuration invalid.</summary>
    ConfigInvalid,

    /// <summary>3: the control endpoint could not be reached (after the child's 5 s connect retry).</summary>
    ControlUnavailable,

    /// <summary>4: the secret or the first message was wrong.</summary>
    Unauthorized,

    /// <summary>5: no common protocol version.</summary>
    ProtoUnsupported,

    /// <summary>6: the public profile's socket or budget (VM only).</summary>
    StartFailed,

    /// <summary>7: the binary does not implement the role it was given yet (<c>ds.role.unimplemented</c>); a build mismatch, never a crash.</summary>
    RoleUnimplemented,

    /// <summary>70: an internal fault outside any world.</summary>
    Fault,

    /// <summary><c>hello.build.version</c> is not the runtime descriptor's (§10); the launcher stopped it.</summary>
    VersionMismatch,

    /// <summary>The child wrote a line that is not JSON or over 1 MiB, on stdout or the channel; the launcher killed it (treated as a crash).</summary>
    ProtocolViolation,

    /// <summary>The launcher stopped it (dispose, a failed start step it owns).</summary>
    Stopped,

    /// <summary>The child started but never connected (or never answered <c>hello</c>) in time; the launcher stopped it. One retry is allowed before hosting is unavailable.</summary>
    ConnectTimedOut,

    /// <summary>A write to the channel stalled (it may have left half a line); the launcher killed the child (§4.6).</summary>
    Stalled,

    /// <summary>Any other code (an access violation, a kill from outside).</summary>
    Crashed,
}

/// <summary>A process end: its exit code when known and what it means to the launcher.</summary>
public sealed record DsExit(DsExitKind Kind, int? Code, string Detail)
{
    /// <summary>§2.1: "hosting unavailable" for the session (no retry would help).</summary>
    public bool HostingUnavailable => Kind is DsExitKind.ConfigInvalid or DsExitKind.ControlUnavailable or DsExitKind.Unauthorized
        or DsExitKind.ProtoUnsupported or DsExitKind.StartFailed or DsExitKind.RoleUnimplemented or DsExitKind.VersionMismatch;

    /// <summary>Counts toward "repeated crashes → hosting unavailable" (§2.2).</summary>
    public bool Crash => Kind is DsExitKind.Fault or DsExitKind.ProtocolViolation or DsExitKind.Crashed or DsExitKind.ConnectTimedOut or DsExitKind.Stalled;

    /// <summary>The toast id the controller shows for a world that was open: always <c>worlds.host_ended</c> (§2.2), never a re-open.</summary>
    public const string HostEndedToast = "worlds.host_ended";

    /// <summary>The §2.1 table, for a process that ended on its own.</summary>
    public static DsExit FromCode(int code) => code switch
    {
        0 => new(DsExitKind.Clean, code, "clean stop"),
        2 => new(DsExitKind.ConfigInvalid, code, "ds.config.invalid"),
        3 => new(DsExitKind.ControlUnavailable, code, "ds.control.unavailable"),
        4 => new(DsExitKind.Unauthorized, code, "ds.control.unauthorized"),
        5 => new(DsExitKind.ProtoUnsupported, code, "ds.control.proto_unsupported"),
        6 => new(DsExitKind.StartFailed, code, "ds.start.failed"),
        7 => new(DsExitKind.RoleUnimplemented, code, "ds.role.unimplemented"),
        70 => new(DsExitKind.Fault, code, "ds.fault"),
        _ => new(DsExitKind.Crashed, code, "exit " + code.ToString(System.Globalization.CultureInfo.InvariantCulture)),
    };
}

/// <summary>
/// The session's hosting availability (§2.1, §2.2): an exit that makes hosting unavailable ends it at once; crashes end
/// it after <see cref="CrashLimit"/>. Never re-opens a world: that stays the player's choice.
/// </summary>
public sealed class DsHostingHealth(int crashLimit = 2)
{
    private readonly object _gate = new();
    private int _crashes;
    private DsExit? _reason;

    public int CrashLimit { get; } = crashLimit > 0 ? crashLimit : throw new ArgumentOutOfRangeException(nameof(crashLimit));

    public bool Available { get { lock (_gate) { return _reason is null; } } }

    /// <summary>The exit that made hosting unavailable; null while available.</summary>
    public DsExit? Reason { get { lock (_gate) { return _reason; } } }

    public int Crashes { get { lock (_gate) { return _crashes; } } }

    public void Record(DsExit exit)
    {
        ArgumentNullException.ThrowIfNull(exit);
        lock (_gate)
        {
            if (exit.Crash)
            {
                _crashes++;
            }
            if (_reason is null && (exit.HostingUnavailable || _crashes >= CrashLimit))
            {
                _reason = exit;
            }
        }
    }
}

/// <summary><c>worlds serve --probe-bind &lt;port&gt;</c>'s outcome (§2.4).</summary>
public enum DsProbeResult
{
    /// <summary>0: bound (and released after 1 s).</summary>
    Bound,

    /// <summary>3: the port is taken (<c>lobby.port_in_use</c>).</summary>
    InUse,

    /// <summary>4: binding was denied (<c>lobby.port_in_use</c>, as <c>AccessDenied</c> today).</summary>
    Denied,

    /// <summary>1 or any other code.</summary>
    Other,

    /// <summary>The exe is missing or could not be started.</summary>
    NotStarted,

    /// <summary>No exit within the bound (killed).</summary>
    TimedOut,
}

public static class DsProbe
{
    public static DsProbeResult FromExitCode(int code) => code switch
    {
        0 => DsProbeResult.Bound,
        3 => DsProbeResult.InUse,
        4 => DsProbeResult.Denied,
        _ => DsProbeResult.Other,
    };

    /// <summary>The launcher's toast for a refused bind, as <c>A/ShellViewModel.Dedicated.cs</c> maps it today.</summary>
    public static string? Toast(DsProbeResult result) => result is DsProbeResult.InUse or DsProbeResult.Denied ? "lobby.port_in_use" : null;
}
