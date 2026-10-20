// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using Infinity.Core.Wire;

namespace Infinity.Core.Session;

/// <summary>
/// The launcher → hook handoff (ADR 0101; docs/launcher/ONLINE_OFFLINE.md §3; session.rs). The
/// launcher spawns <c>azimuth.exe</c> with <c>FI_LAUNCH=v1;pipe=&lt;nonce&gt;</c>; the hook connects to
/// <c>\\.\pipe\ForzaInfinity-&lt;nonce&gt;</c>, sends <see cref="Frame.Hello"/> and receives
/// <see cref="Frame.Ticket"/>.
/// </summary>
public static class Launch
{
    public const string EnvLaunch = "FI_LAUNCH";

    /// <summary><c>FI_LAUNCH</c> for "Play offline": hooks from build 59 force offline (ADR 0101 §8).</summary>
    public const string EnvOffline = "v1;offline";

    /// <summary>The launcher's own exe, reserved for the hook-side respawn (ORCHESTRATOR §1.6, CR-L19).</summary>
    public const string EnvLauncherExe = "FI_LAUNCHER_EXE";

    public const ushort ProtocolVersion = 1;

    public const string PipePrefix = @"\\.\pipe\ForzaInfinity-";

    /// <summary>The first hook build that speaks this protocol (<c>0.3.x+fh2online.60</c>, ORCHESTRATOR §5).</summary>
    public const uint FirstProtocolBuild = 60;

    /// <summary>The one-time session file of the file handoff, in data/bin.</summary>
    public const string SessionFile = "FI_Session.v1";

    public const int NonceLength = 16;

    public static byte[] NewNonce() => RandomNumberGenerator.GetBytes(NonceLength);

    /// <summary>
    /// What a hook does with its <c>FI_LAUNCH</c> value: null is forced offline
    /// (<see cref="EnvOffline"/>), otherwise online through that channel. Throws for anything else
    /// (the hook plays offline with a notice).
    /// </summary>
    public static Handoff? ParseLaunchEnv(string value) => value == EnvOffline ? null : Handoff.ParseEnv(value);

    /// <summary>The build number of a hook version (<c>0.3.0+fh2online.60</c> → 60; hook.rs <c>build_number</c>).</summary>
    public static uint? BuildNumber(string hookVersion)
    {
        var at = hookVersion.LastIndexOf("+fh2online.", StringComparison.Ordinal);
        if (at < 0)
        {
            return null;
        }
        var digits = hookVersion[(at + "+fh2online.".Length)..];
        // Rust's u32 parse takes one leading '+' (and no '-').
        if (digits.StartsWith('+'))
        {
            digits = digits[1..];
        }
        return uint.TryParse(digits, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var n)
            ? n
            : null;
    }
}

public enum HandoffChannel
{
    Pipe,
    File,
}

/// <summary>How this launch hands the session over, and its 16-byte nonce.</summary>
public sealed record Handoff
{
    private readonly byte[] _nonce;

    public Handoff(HandoffChannel channel, ReadOnlySpan<byte> nonce)
    {
        if (nonce.Length != Launch.NonceLength)
        {
            throw new ProtocolException("bad nonce");
        }
        Channel = channel;
        _nonce = nonce.ToArray();
    }

    public HandoffChannel Channel { get; }

    public ReadOnlySpan<byte> Nonce => _nonce;

    public string NonceHex => Hex.Encode(_nonce);

    /// <summary>The <c>FI_LAUNCH</c> value.</summary>
    public string EnvValue => Channel == HandoffChannel.Pipe ? $"v1;pipe={NonceHex}" : $"v1;file={NonceHex}";

    public string PipeName => Launch.PipePrefix + NonceHex;

    /// <summary>The overlay's UI pipe (ORCHESTRATOR §4.3): same DACL and PID rule, its own frame space.</summary>
    public string UiPipeName => PipeName + "-ui";

    /// <summary>Parses an online <c>FI_LAUNCH</c> value (<c>v1;offline</c> is not one: see <see cref="Launch.ParseLaunchEnv"/>).</summary>
    public static Handoff ParseEnv(string value)
    {
        ArgumentNullException.ThrowIfNull(value);
        if (!value.StartsWith("v1;", StringComparison.Ordinal))
        {
            throw new ProtocolException("FI_LAUNCH is not v1");
        }
        var rest = value[3..];
        var eq = rest.IndexOf('=', StringComparison.Ordinal);
        if (eq < 0)
        {
            throw new ProtocolException("FI_LAUNCH has no channel");
        }
        var bytes = Hex.Decode(rest[(eq + 1)..]);
        if (bytes is null || bytes.Length != Launch.NonceLength)
        {
            throw new ProtocolException("bad nonce");
        }
        return rest[..eq] switch
        {
            "pipe" => new Handoff(HandoffChannel.Pipe, bytes),
            "file" => new Handoff(HandoffChannel.File, bytes),
            var other => throw new ProtocolException($"unknown channel {other}"),
        };
    }

    public bool Equals(Handoff? other) => other is not null && Channel == other.Channel && _nonce.AsSpan().SequenceEqual(other._nonce);

    public override int GetHashCode() => HashCode.Combine(Channel, NonceHex);
}
