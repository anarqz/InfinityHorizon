// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;
using System.Text.RegularExpressions;

namespace Infinity.Core.Process;

/// <summary>
/// An Azimuth folder (game::fh2 in the Rust launcher): <c>azimuth.exe</c> (Azimuth's launcher
/// and the game host), <c>data\bin</c> (the hook's files), the Azimuth version and the SHA-256 of
/// <c>azimuth.exe</c> (OD-125: one exact build per release), and the installed hook's version and
/// SHA-256 (the gate logs it; none installed = SHA-256 of zero bytes, CR-L20). The hook is
/// ExtraOptions.Infinity, the <c>winmm.dll</c> beside <c>azimuth.exe</c> (OD-125). Read only:
/// nothing here writes the folder.
/// </summary>
public sealed partial record AzimuthInstall(string Root, string HostExe, string Bin, string? Version, string? HookVersion, string HookSha256, string HostSha256)
{
    public const string HookFile = "winmm.dll";

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
        var hook = Path.Combine(root, HookFile);
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
        var host = File.ReadAllBytes(exe);
        return new AzimuthInstall(root, exe, bin, ReadVersion(host), hookVersion, hookSha, Convert.ToHexStringLower(SHA256.HashData(host)));
    }

    /// <summary>The <c>"Azimuth x.y.z"</c> tag Azimuth carries in UTF-16 (0.9.x and 1.0.x).</summary>
    private static string? ReadVersion(byte[] exe)
    {
        var text = Encoding.Unicode.GetString(exe);
        // 0.9.x carried the bare number: still read, so the setup screen can name what it found
        return AzimuthVersionPattern().Match(text) is { Success: true } m ? m.Groups[1].Value
            : LegacyVersionPattern().Match(text) is { Success: true } legacy ? legacy.Groups[1].Value : null;
    }

    [GeneratedRegex(@"\d+\.\d+\.\d+\+fh2online\.\d+")]
    private static partial Regex HookVersionPattern();

    [GeneratedRegex(@"Azimuth (\d+\.\d+\.\d+)")]
    private static partial Regex AzimuthVersionPattern();

    [GeneratedRegex(@"(0\.9\.\d+)")]
    private static partial Regex LegacyVersionPattern();
}
