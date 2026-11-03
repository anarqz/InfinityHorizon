// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using Infinity.Core.Process;
using Org.BouncyCastle.Math.EC.Rfc8032;

namespace Infinity.Core.DedicatedServer;

/// <summary>
/// Which signature <c>worlds.exe</c> must carry before the launcher runs it or writes its firewall rule
/// (<see cref="DsSignature"/>). A production build accepts the release key only; a development build may also accept the
/// development key of a development server (<see cref="HookInstaller.DevKeyEnv"/>, as the overlay and the hook), never an
/// unsigned file.
/// </summary>
public sealed record DsSignaturePolicy
{
    /// <summary>The release key only (production and custom servers).</summary>
    public static DsSignaturePolicy Official { get; } = new() { OfficialOnly = true };

    /// <summary>True: the release key only, whatever <see cref="DevelopmentKey"/> says.</summary>
    public bool OfficialOnly { get; init; } = true;

    /// <summary>A development server's key (<c>FI_DEV_HOOK_KEY</c>); ignored in a production build and when <see cref="OfficialOnly"/>.</summary>
    public byte[]? DevelopmentKey { get; init; }

#if !INFINITY_PRODUCTION_BUILD
    /// <summary>Tests only: this key stands for the release key (a test cannot sign with the real one).</summary>
    internal byte[]? TestOnlyReleaseKey { get; init; }

    /// <summary>Tests only: the VERSIONINFO a signed stand-in carries instead of the world server's.</summary>
    internal (string OriginalFilename, string FileDescription)? TestOnlyIdentity { get; init; }

    /// <summary>Tests only: an unsigned exe (the fake) runs.</summary>
    internal bool TestOnlyUnsigned { get; init; }
#endif
}

/// <summary>
/// An exe whose signature verified, held open with its <c>.sig</c> (share read only: no write, delete or rename) until it
/// has been started, so the file that runs is the file that was checked.
/// </summary>
public sealed class DsVerifiedExecutable : IDisposable
{
    private readonly FileStream? _exe;
    private readonly FileStream? _signature;

    internal DsVerifiedExecutable(string path, FileStream? exe, FileStream? signature)
    {
        Path = path;
        _exe = exe;
        _signature = signature;
    }

    public string Path { get; }

    public void Dispose()
    {
        _exe?.Dispose();
        _signature?.Dispose();
    }
}

/// <summary>
/// The world server's run check (DEDICATED_SERVER.md §2.4): its detached <c>.sig</c> verifies with the release key over the
/// launcher's signature line (<see cref="HookInstaller.SignPrefix"/> and the SHA-256, as <see cref="ReleaseSignature"/> and
/// <c>hooksign release-sign</c>), computed from the very handle that stays open until the start; and, because that line
/// names no role (any release-signed file renamed would pass), its VERSIONINFO says it is the world server:
/// <c>OriginalFilename</c> <see cref="OriginalFilename"/> and <c>FileDescription</c> <see cref="FileDescription"/>.
/// </summary>
public static class DsSignature
{
    public const string OriginalFilename = "worlds.exe";

    public const string FileDescription = "Infinity Worlds";

    /// <summary>Whether <paramref name="executable"/> would run now (the handles are released at once).</summary>
    public static bool Trusted(string executable, DsSignaturePolicy policy, Action<string>? why = null)
    {
        using var verified = Open(executable, policy, why);
        return verified is not null;
    }

    /// <summary>
    /// The verified exe, held open until disposed; null when it is missing, unreadable, tampered, signed by another key or not
    /// the world server. <paramref name="why"/> gets the reason (no path, no hash).
    /// </summary>
    public static DsVerifiedExecutable? Open(string executable, DsSignaturePolicy policy, Action<string>? why = null)
    {
        ArgumentNullException.ThrowIfNull(executable);
        ArgumentNullException.ThrowIfNull(policy);
#if !INFINITY_PRODUCTION_BUILD
        if (policy.TestOnlyUnsigned) return new DsVerifiedExecutable(executable, null, null);
#endif
        FileStream? exe = null, signature = null;
        try
        {
            exe = new FileStream(executable, FileMode.Open, FileAccess.Read, FileShare.Read);
            signature = new FileStream(executable + ".sig", FileMode.Open, FileAccess.Read, FileShare.Read);
            if (signature.Length != 64 || exe.Length is <= 0 or > ReleaseSignature.MaxFileBytes)
                return Refuse("the signature or the file has the wrong size");
            var sig = new byte[64];
            signature.ReadExactly(sig);
            var line = Encoding.ASCII.GetBytes(HookInstaller.SignPrefix + Convert.ToHexStringLower(SHA256.HashData(exe)));
            if (!Signed(policy, sig, line)) return Refuse("the signature does not verify");
            // the signature line names no role: the file must also say it is the world server (a renamed release file is not)
            var info = FileVersionInfo.GetVersionInfo(executable);
            var expected = (OriginalFilename, FileDescription);
#if !INFINITY_PRODUCTION_BUILD
            expected = policy.TestOnlyIdentity ?? expected;
#endif
            if (!string.Equals(info.OriginalFilename, expected.Item1, StringComparison.OrdinalIgnoreCase)
                || !string.Equals(info.FileDescription, expected.Item2, StringComparison.Ordinal))
                return Refuse("its VERSIONINFO does not name the world server");
            var verified = new DsVerifiedExecutable(executable, exe, signature);
            (exe, signature) = (null, null);
            return verified;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException)
        {
            return Refuse(e is FileNotFoundException ? "the file or its signature is missing" : "it cannot be read (" + e.GetType().Name + ")");
        }
        finally
        {
            exe?.Dispose();
            signature?.Dispose();
        }

        DsVerifiedExecutable? Refuse(string reason)
        {
            why?.Invoke(reason);
            return null;
        }
    }

    private static bool Signed(DsSignaturePolicy policy, byte[] signature, byte[] line)
    {
        var release = HookInstaller.ReleaseKey.ToArray();
#if !INFINITY_PRODUCTION_BUILD
        release = policy.TestOnlyReleaseKey ?? release;
#endif
        if (Verify(release, signature, line)) return true;
#if INFINITY_PRODUCTION_BUILD
        _ = policy; // a production build: the release key only
        return false;
#else
        return !policy.OfficialOnly && policy.DevelopmentKey is { Length: 32 } development && Verify(development, signature, line);
#endif
    }

    private static bool Verify(byte[] key, byte[] signature, byte[] line) =>
        key.Length == 32 && Ed25519.ValidatePublicKeyFull(key, 0) && Ed25519.Verify(signature, 0, key, 0, line, 0, line.Length);
}
