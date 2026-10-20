// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;
using System.Text.RegularExpressions;

namespace Infinity.Core.Process;

/// <summary>
/// An Azimuth folder (game::fh2 in the Rust launcher): <c>azimuth.exe</c> (Azimuth's Start window
/// and the game host), <c>data\bin</c> (where the hook is <c>azimuth-tls.dll</c>), the Azimuth
/// version read from the exe, and the installed hook's version and SHA-256 (the gate logs it;
/// none installed = SHA-256 of zero bytes, CR-L20). Read only: nothing here writes the folder.
/// </summary>
public sealed partial record AzimuthInstall(string Root, string HostExe, string Bin, string? Version, string? HookVersion, string HookSha256)
{
    public const string HookFile = "azimuth-tls.dll";

    /// <summary>The folder's install, or null when it holds no <c>azimuth.exe</c>.</summary>
    public static AzimuthInstall? Detect(string root)
    {
        ArgumentNullException.ThrowIfNull(root);
        var exe = Path.Combine(root, "azimuth.exe");
        if (!File.Exists(exe))
        {
            return null;
        }
        var bin = Path.Combine(root, "data", "bin");
        var hook = Path.Combine(bin, HookFile);
        string? hookVersion = null;
        string hookSha;
        if (File.Exists(hook))
        {
            var bytes = File.ReadAllBytes(hook);
            hookSha = Convert.ToHexStringLower(SHA256.HashData(bytes));
            // The banner `ForzaInfinity <version>` is the first +fh2online.<n> of the file (BUILD.md).
            hookVersion = HookVersionPattern().Match(Encoding.ASCII.GetString(bytes)) is { Success: true } m ? m.Value : null;
        }
        else
        {
            hookSha = Convert.ToHexStringLower(SHA256.HashData([]));
        }
        return new AzimuthInstall(root, exe, bin, ReadVersion(exe), hookVersion, hookSha);
    }

    /// <summary>The <c>" Azimuth 0.9.x "</c> tag Azimuth carries in UTF-16.</summary>
    private static string? ReadVersion(string exe)
    {
        var text = Encoding.Unicode.GetString(File.ReadAllBytes(exe));
        return AzimuthVersionPattern().Match(text) is { Success: true } m ? m.Groups[1].Value : null;
    }

    [GeneratedRegex(@"\d+\.\d+\.\d+\+fh2online\.\d+")]
    private static partial Regex HookVersionPattern();

    [GeneratedRegex(@"(0\.9\.\d+)")]
    private static partial Regex AzimuthVersionPattern();
}
