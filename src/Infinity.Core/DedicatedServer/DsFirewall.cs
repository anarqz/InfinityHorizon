// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Runtime.InteropServices;
using System.Runtime.Versioning;
using Infinity.Core.Process;

namespace Infinity.Core.DedicatedServer;

/// <summary>What Windows Firewall does to inbound UDP for the exe on the active profile (DEDICATED_SERVER.md §2.4).</summary>
public enum DsFirewallState
{
    /// <summary>Not readable here: not Windows, under Wine/Proton (no Windows Firewall), or the COM read failed.</summary>
    Unknown,

    /// <summary>Every active profile has the firewall off.</summary>
    Disabled,

    /// <summary>An enabled inbound Allow rule names the exe on an active profile (UDP or any protocol), and no Block rule.</summary>
    Allowed,

    /// <summary>An enabled inbound Block rule names the exe on an active profile (it wins over any Allow).</summary>
    Blocked,

    /// <summary>An active profile blocks all inbound traffic ("block all incoming connections"): no Allow rule helps, so the helper is not offered; the player is told the profile blocks all inbound.</summary>
    BlockAllInbound,

    /// <summary>No rule names the exe on an active profile: the first bind may prompt, joiners may not reach the port.</summary>
    NoAllowRule,
}

public sealed record DsFirewallStatus(DsFirewallState State, string Detail)
{
    /// <summary>§2.4: offer the optional "Allow hosting through Windows Firewall" (EN/pt-BR); declining keeps hosting available.</summary>
    public bool OfferRule => State is DsFirewallState.Blocked or DsFirewallState.NoAllowRule;

    /// <summary>
    /// The player-facing message (EN/pt-BR strings land with the stage 2 UI): the Allow-rule offer, or, when the profile
    /// blocks all inbound traffic, a different message (no rule helps; the player would change the profile setting).
    /// Null when nothing needs saying.
    /// </summary>
    public string? MessageKey => State switch
    {
        DsFirewallState.Blocked or DsFirewallState.NoAllowRule => "worlds.firewall_allow_offer",
        DsFirewallState.BlockAllInbound => "worlds.firewall_blocks_all_inbound",
        _ => null,
    };
}

/// <summary>One firewall rule as the reader sees it (INetFwRule's fields that matter here).</summary>
public sealed record DsFirewallRule(string? ApplicationName, bool Allow, bool Inbound, bool Enabled, int Profiles, int Protocol, string? LocalPorts = null);

/// <summary>
/// The read-only firewall check (§2.4): <c>INetFwPolicy2</c> through COM, no elevation. A stub elsewhere: off Windows
/// and under Wine/Proton it reports <see cref="DsFirewallState.Unknown"/>. Writing a rule is the elevated helper's
/// (<see cref="DsFirewallRuleCommand"/>), never this.
/// </summary>
public static class DsFirewall
{
    public const int ProfileDomain = 1;
    public const int ProfilePrivate = 2;
    public const int ProfilePublic = 4;
    public const int ProtocolUdp = 17;
    public const int ProtocolAny = 256;

    public static DsFirewallStatus Read(string executable, int port = Matchmaking.HostRelay.DefaultPort)
    {
        ArgumentNullException.ThrowIfNull(executable);
        if (!OperatingSystem.IsWindows())
        {
            return new(DsFirewallState.Unknown, "not windows");
        }
        if (WineHost.Detected)
        {
            return new(DsFirewallState.Unknown, "wine");
        }
        return ReadWindows(executable, port);
    }

    /// <summary>
    /// The decision over a rule list (pure: the tests' entry). <paramref name="blockAllInbound"/> (a profile's "block all
    /// incoming connections") wins over every Allow; a rule counts only when enabled, inbound, on an active profile, UDP
    /// or any protocol, naming the exe, and (when it names local ports) covering <paramref name="port"/>.
    /// </summary>
    public static DsFirewallStatus Evaluate(string executable, int activeProfiles, IEnumerable<DsFirewallRule> rules, bool? enabledOnActive = null,
        int port = Matchmaking.HostRelay.DefaultPort, bool blockAllInbound = false)
    {
        ArgumentNullException.ThrowIfNull(rules);
        if (enabledOnActive == false)
        {
            return new(DsFirewallState.Disabled, "firewall off on the active profiles");
        }
        if (blockAllInbound)
        {
            return new(DsFirewallState.BlockAllInbound, "the active profile blocks all inbound traffic");
        }
        var exe = Normalize(executable);
        var allowed = false;
        foreach (var rule in rules)
        {
            if (rule is null || !rule.Enabled || !rule.Inbound || (rule.Profiles & activeProfiles) == 0 || rule.Protocol is not (ProtocolUdp or ProtocolAny)
                || rule.ApplicationName is not { Length: > 0 } app || !string.Equals(Normalize(app), exe, StringComparison.OrdinalIgnoreCase)
                || !CoversPort(rule.LocalPorts, port))
            {
                continue;
            }
            if (!rule.Allow)
            {
                return new(DsFirewallState.Blocked, "a block rule names the exe");
            }
            allowed = true;
        }
        return allowed ? new(DsFirewallState.Allowed, "an allow rule names the exe") : new(DsFirewallState.NoAllowRule, "no rule names the exe");
    }

    /// <summary><c>INetFwRule.LocalPorts</c>: empty or <c>*</c> is every port; else a list of ports and ranges (keywords such as <c>RPC</c> cover none).</summary>
    public static bool CoversPort(string? localPorts, int port)
    {
        if (string.IsNullOrWhiteSpace(localPorts) || localPorts.Trim() == "*")
        {
            return true;
        }
        foreach (var part in localPorts.Split(',', StringSplitOptions.RemoveEmptyEntries | StringSplitOptions.TrimEntries))
        {
            var dash = part.IndexOf('-', StringComparison.Ordinal);
            if (dash < 0)
            {
                if (int.TryParse(part, NumberStyles.None, CultureInfo.InvariantCulture, out var single) && single == port)
                {
                    return true;
                }
            }
            else if (int.TryParse(part[..dash], NumberStyles.None, CultureInfo.InvariantCulture, out var low)
                && int.TryParse(part[(dash + 1)..], NumberStyles.None, CultureInfo.InvariantCulture, out var high) && port >= low && port <= high)
            {
                return true;
            }
        }
        return false;
    }

    private static string Normalize(string path)
    {
        try
        {
            return Path.GetFullPath(Environment.ExpandEnvironmentVariables(path.Trim().Trim('"')));
        }
        catch (Exception e) when (e is ArgumentException or NotSupportedException or PathTooLongException)
        {
            return path;
        }
    }

    [SupportedOSPlatform("windows")]
    private static void Release(object? com)
    {
        if (com is not null && Marshal.IsComObject(com))
        {
            Marshal.FinalReleaseComObject(com);
        }
    }

    [SupportedOSPlatform("windows")]
    private static DsFirewallStatus ReadWindows(string executable, int port)
    {
        object? policy = null;
        object? collection = null;
        System.Collections.IEnumerator? enumerator = null;
#pragma warning disable CA1031 // a firewall read never fails hosting: anything unexpected is "unknown"
        try
        {
            var type = Type.GetTypeFromProgID("HNetCfg.FwPolicy2", throwOnError: false);
            if (type is null)
            {
                return new(DsFirewallState.Unknown, "no HNetCfg.FwPolicy2");
            }
            policy = Activator.CreateInstance(type);
            dynamic fw = policy!;
            int active = fw.CurrentProfileTypes;
            bool? enabled = null;
            var blockAll = false;
            try
            {
                var any = false;
                foreach (var profile in new[] { ProfileDomain, ProfilePrivate, ProfilePublic })
                {
                    if ((active & profile) == 0)
                    {
                        continue;
                    }
                    var on = (bool)fw.FirewallEnabled[profile];
                    any |= on;
                    blockAll |= on && (bool)fw.BlockAllInboundTraffic[profile];
                }
                enabled = any;
            }
            catch (Exception)
            {
                (enabled, blockAll) = (null, false); // unreadable: judge by the rules
            }
            var rules = new List<DsFirewallRule>();
            collection = fw.Rules;
            enumerator = ((System.Collections.IEnumerable)collection).GetEnumerator();
            while (enumerator.MoveNext())
            {
                dynamic rule = enumerator.Current!;
                try
                {
                    rules.Add(new DsFirewallRule((string?)rule.ApplicationName, (int)rule.Action == 1, (int)rule.Direction == 1, (bool)rule.Enabled,
                        (int)rule.Profiles, (int)rule.Protocol, (string?)rule.LocalPorts));
                }
                catch (Exception)
                {
                    // a rule that cannot be read is not counted
                }
                finally
                {
                    Release((object)rule);
                }
            }
            return Evaluate(executable, active, rules, enabled, port, blockAll);
        }
        catch (Exception e)
        {
            return new(DsFirewallState.Unknown, "read failed: " + e.GetType().Name);
        }
        finally
        {
            // the runtime wraps the collection's IEnumVARIANT in an adapter: release the COM enumerator under it
            if (enumerator is ICustomAdapter adapter)
            {
                Release(adapter.GetUnderlyingObject());
            }
            Release(collection);
            Release(policy);
        }
#pragma warning restore CA1031
    }
}
/// <summary>
/// The elevated helper's entry point (§2.4; stage 2 wires it into the UI): <c>infinity.exe --firewall-rule --exe
/// &lt;path&gt; [--public]</c>, started with UAC elevation, adds one inbound Allow rule for the exe (UDP, profile
/// Private, plus Public only when the player opts in) and removes the Block rules naming it. The launcher never
/// elevates otherwise; the updater never does.
/// </summary>
public sealed record DsFirewallRuleCommand(string Executable, bool IncludePublic)
{
    public const string Switch = "--firewall-rule";

    /// <summary>The rule's name and what the firewall list shows (the exe's VERSIONINFO <c>FileDescription</c>).</summary>
    public const string RuleName = "Infinity world server";

    public int Profiles => DsFirewall.ProfilePrivate | (IncludePublic ? DsFirewall.ProfilePublic : 0);

    public IReadOnlyList<string> ToArguments()
    {
        var args = new List<string> { Switch, "--exe", Executable };
        if (IncludePublic)
        {
            args.Add("--public");
        }
        return args;
    }

    /// <summary>Parses the helper's arguments; null when they are not exactly <c>--firewall-rule --exe &lt;absolute path to dedicated-server.exe&gt; [--public]</c>.</summary>
    public static DsFirewallRuleCommand? TryParse(IReadOnlyList<string> args)
    {
        ArgumentNullException.ThrowIfNull(args);
        if (args.Count is < 3 or > 4 || args[0] != Switch || args[1] != "--exe")
        {
            return null;
        }
        var exe = args[2];
        if (!Path.IsPathFullyQualified(exe) || !string.Equals(Path.GetFileName(exe), DedicatedServerExe, StringComparison.OrdinalIgnoreCase))
        {
            return null;
        }
        var includePublic = false;
        if (args.Count == 4)
        {
            if (args[3] != "--public")
            {
                return null;
            }
            includePublic = true;
        }
        return new DsFirewallRuleCommand(Path.GetFullPath(exe), includePublic);
    }

    /// <summary>
    /// As <see cref="TryParse(IReadOnlyList{string})"/>, and null unless the named exe carries a signature
    /// <paramref name="signature"/> accepts: the elevated helper never writes a rule for a file the launcher would not run.
    /// </summary>
    public static DsFirewallRuleCommand? TryParse(IReadOnlyList<string> args, DsSignaturePolicy signature)
    {
        ArgumentNullException.ThrowIfNull(signature);
        return TryParse(args) is { } command && DsSignature.Trusted(command.Executable, signature) ? command : null;
    }

    /// <summary>
    /// The helper's apply step (stage 3) starts here: the command and its exe, verified and held open (no write, delete or
    /// rename) until the rule is written (<paramref name="verified"/> is disposed after it). Null when either check fails.
    /// </summary>
    public static DsFirewallRuleCommand? TryOpen(IReadOnlyList<string> args, DsSignaturePolicy signature, out DsVerifiedExecutable? verified)
    {
        ArgumentNullException.ThrowIfNull(signature);
        verified = TryParse(args) is { } command ? DsSignature.Open(command.Executable, signature) : null;
        return verified is null ? null : TryParse(args);
    }

    private const string DedicatedServerExe = "dedicated-server.exe";

    public override string ToString() => string.Create(CultureInfo.InvariantCulture, $"firewall rule: allow inbound UDP, profiles {Profiles}");
}
