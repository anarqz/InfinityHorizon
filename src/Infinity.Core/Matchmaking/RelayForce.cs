// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Matchmaking;

/// <summary>
/// <c>FI_RELAY_FORCE=1</c>, a test switch to prove the relay end to end on a network where direct always works (CR-S57): the
/// launcher's joins go straight to the relay their grant names (<see cref="WorldsBridge.ForceRelay"/>). Read here only, once per
/// process; any other value, or none, changes nothing. OD-160: like <see cref="WatchTestSwitch"/>, only a build whose version has a
/// SemVer prerelease reads it; a released launcher ignores it (never a player setting, OD-144).
/// </summary>
public static class RelayForce
{
    public const string Variable = "FI_RELAY_FORCE";
    private static readonly Lazy<bool> Set = new(() => IsSet(Environment.GetEnvironmentVariable(Variable),
        System.Reflection.CustomAttributeExtensions.GetCustomAttribute<System.Reflection.AssemblyInformationalVersionAttribute>(typeof(RelayForce).Assembly)?.InformationalVersion));
    private static int _logged;

    /// <summary>Exactly "1" (surrounding blanks allowed) turns it on.</summary>
    public static bool IsSet(string? value) => value?.Trim() == "1";

    /// <summary><see cref="IsSet(string?)"/>, and only in a prerelease build (<paramref name="informationalVersion"/> with a SemVer prerelease).</summary>
    public static bool IsSet(string? value, string? informationalVersion) =>
        IsSet(value) && Update.LauncherVersion.TryParse(informationalVersion, out var version) && version.Prerelease is not null;

    /// <summary>Whether the switch is on; said once to <paramref name="log"/> the first time it is.</summary>
    public static bool Enabled(Action<string>? log)
    {
        if (!Set.Value) return false;
        if (Interlocked.Exchange(ref _logged, 1) == 0) log?.Invoke("relay: FI_RELAY_FORCE set (prerelease build): joins go straight to the relay");
        return true;
    }
}
