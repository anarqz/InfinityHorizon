// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Runtime.Versioning;
using Infinity.Core.Presence;

namespace Infinity.Core.Process;

/// <summary>The few registry operations the link registration needs, behind an interface so its rules test without a registry.</summary>
public interface IUserClassesKey
{
    /// <summary>The value (<c>""</c>: the key's default) of <c>HKCU\Software\Classes\&lt;subkey&gt;</c>, or null when absent.</summary>
    string? Read(string subkey, string name);

    void Write(string subkey, string name, string value);
}

/// <summary>
/// CR-L41.2: <c>infinity-horizon://</c> for the current user only (<c>HKCU\Software\Classes\infinity-horizon</c>, never HKLM); the
/// command is this launcher's exe with <c>"%1"</c>. Written only by a release build, at startup, and only the values that differ
/// (idempotent: a second start writes nothing). A copy moved elsewhere re-points the link to itself.
/// </summary>
public static class ProtocolRegistration
{
    public const string Key = JoinLink.Scheme;

    public static string Command(string exePath) => "\"" + exePath + "\" \"%1\"";

    /// <summary>The values to hold: (subkey, name, value).</summary>
    public static IReadOnlyList<(string Subkey, string Name, string Value)> Values(string exePath)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(exePath);
        if (!Path.IsPathFullyQualified(exePath) || exePath.Contains('"', StringComparison.Ordinal)) throw new ArgumentException("not a full exe path", nameof(exePath));
        return
        [
            (Key, "", "URL:Infinity Horizon"),
            (Key, "URL Protocol", ""),
            (Key + @"\DefaultIcon", "", "\"" + exePath + "\",0"),
            (Key + @"\shell\open\command", "", Command(exePath)),
        ];
    }

    /// <summary>Writes what differs; returns how many values were written (0: already registered).</summary>
    public static int Ensure(IUserClassesKey classes, string exePath)
    {
        ArgumentNullException.ThrowIfNull(classes);
        var written = 0;
        foreach (var (subkey, name, value) in Values(exePath))
        {
            if (classes.Read(subkey, name) == value) continue;
            classes.Write(subkey, name, value);
            ++written;
        }
        return written;
    }

    /// <summary><c>HKCU\Software\Classes</c> of the current user.</summary>
    [SupportedOSPlatform("windows")]
    public sealed class CurrentUser : IUserClassesKey
    {
        private const string Root = @"Software\Classes\";

        public string? Read(string subkey, string name)
        {
            using var key = Microsoft.Win32.Registry.CurrentUser.OpenSubKey(Root + subkey, writable: false);
            return key?.GetValue(name, null, Microsoft.Win32.RegistryValueOptions.DoNotExpandEnvironmentNames) as string;
        }

        public void Write(string subkey, string name, string value)
        {
            using var key = Microsoft.Win32.Registry.CurrentUser.CreateSubKey(Root + subkey, writable: true);
            key.SetValue(name, value, Microsoft.Win32.RegistryValueKind.String);
        }
    }
}
