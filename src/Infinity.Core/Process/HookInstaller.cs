// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using Org.BouncyCastle.Math.EC.Rfc8032;

namespace Infinity.Core.Process;

/// <summary>A hook the launcher refused (signature, image, size), or an install that could not finish.</summary>
public sealed class HookException(string message) : Exception(message);

/// <summary>Which key signed a hook.</summary>
public enum HookSigner
{
    None,

    /// <summary>The project's release key (embedded): the official hook.</summary>
    Official,

    /// <summary>A development key given by <c>FI_DEV_HOOK_KEY</c> on this PC only.</summary>
    Development,
}

/// <summary>What the hook, <c>winmm.dll</c> beside <c>azimuth.exe</c> (OD-125), is.</summary>
public enum HookKind
{
    Missing,

    /// <summary>Azimuth's own small helper: the game plays without Infinity.</summary>
    Stock,

    /// <summary>An Infinity hook (it carries its <c>+fh2online.N</c> build).</summary>
    Ours,

    /// <summary>Something else (another mod, a damaged file).</summary>
    Unknown,
}

/// <summary>The installed hook: kind, version, SHA-256, and who signed exactly these bytes (the signature the launcher kept when it installed them).</summary>
public sealed record InstalledHook(HookKind Kind, string? Version, string? Sha256, HookSigner Signer)
{
    public bool Verified => Kind == HookKind.Ours && Signer != HookSigner.None;
}

/// <summary>The hook package the launcher carries (<c>hook\infinity.dll</c> + <c>.sig</c>), verified.</summary>
public sealed record HookPackage(string Path, byte[] Bytes, byte[] Signature, string Version, string Sha256, HookSigner Signer);

/// <summary>
/// OD-47: installs the Infinity hook into the player's Azimuth (the C# side of
/// <c>crates/fi-launcher-core/src/hook.rs</c>). A hook is accepted only with an Ed25519 signature
/// (RFC 8032, strict) over the 84-byte line <c>"fh2online-update-v1 " + SHA-256 lowercase hex</c>,
/// the line the hook's own update check verifies (<c>native/hook/src/infinity/sign.c</c>), by the
/// release key below; it must be a PE image (<c>MZ</c>) of at most 8 MiB carrying its
/// <c>x.y.z+fh2online.N</c> build. Installing needs the game closed; Azimuth's own DLL is kept as
/// <c>azimuth-tls.dll.stock</c>, the replaced one as <c>.previous</c>, and the signature of what
/// was installed in <c>data\fi-launcher\hook\</c> so the screen can show it is the official one.
/// </summary>
public static class HookInstaller
{
    /// <summary>OD-125: ExtraOptions.Infinity, the <c>winmm.dll</c> beside <c>azimuth.exe</c> (Azimuth 1.0.x loads no TLS helper for FH2).</summary>
    public const string HookDll = "winmm.dll";
    public const string PackageFile = "infinity.dll";
    public const string SignPrefix = "fh2online-update-v1 ";
    public const string DevKeyEnv = "FI_DEV_HOOK_KEY";
    public const int MaxHookBytes = 8 << 20;
    public const int StockMaxBytes = 64 * 1024;

    /// <summary>Azimuth 0.9.10's own helper (thirdparty.rs pins the same): the only file kept as <c>.stock</c>.</summary>
    public const string Stock0910Sha256 = "d92ff5c173ec42c2f371d8d28a30c5535489f7f0dffa43363860cf1fe70ee695";

    /// <summary>The project's release public key, the same 32 bytes as <c>azimuth_sign_release_key</c> in sign.c.</summary>
    public static ReadOnlySpan<byte> ReleaseKey =>
    [
        0xbf, 0xe5, 0x72, 0xbc, 0x40, 0x36, 0x71, 0x2e, 0x5a, 0x2f, 0x96, 0x83, 0xc6, 0xb9, 0xfe, 0x29,
        0x3d, 0xce, 0x19, 0x62, 0x74, 0xe0, 0x8b, 0x10, 0x83, 0x0e, 0xf0, 0x23, 0x28, 0x60, 0xae, 0xfb,
    ];

    /// <summary>A development key from <c>FI_DEV_HOOK_KEY</c> (64 hex digits), or null.</summary>
    public static byte[]? DevKey(string? hex) =>
        hex is { Length: 64 } && hex.All(Uri.IsHexDigit) ? Convert.FromHexString(hex) : null;

    /// <summary>The build marker's version (<c>0.3.0+fh2online.60</c>), as hook.rs <c>hook_version</c> reads it.</summary>
    public static string? VersionOf(ReadOnlySpan<byte> bytes)
    {
        var marker = "+fh2online."u8;
        var from = 0;
        while (from < bytes.Length && bytes[from..].IndexOf(marker) is var rel and >= 0)
        {
            var at = from + rel;
            var start = at;
            while (start > 0 && (char.IsAsciiDigit((char)bytes[start - 1]) || bytes[start - 1] == '.'))
            {
                start--;
            }
            var end = at + marker.Length;
            while (end < bytes.Length && char.IsAsciiDigit((char)bytes[end]))
            {
                end++;
            }
            var head = bytes[start..at];
            if (end > at + marker.Length && head.Count((byte)'.') == 2 && head.Length > 0 && char.IsAsciiDigit((char)head[0]))
            {
                return Encoding.ASCII.GetString(bytes[start..end]);
            }
            from = at + 1;
        }
        return null;
    }

    /// <summary>N of <c>x.y.z+fh2online.N</c>.</summary>
    public static int? Build(string? version) =>
        version is not null && version.LastIndexOf("+fh2online.", StringComparison.Ordinal) is var at and >= 0
        && int.TryParse(version.AsSpan(at + 11), NumberStyles.None, CultureInfo.InvariantCulture, out var n) ? n : null;

    /// <summary>Who signed <paramref name="sha256Hex"/>'s update line (None: neither key, or a bad signature).</summary>
    public static HookSigner SignerOf(string sha256Hex, ReadOnlySpan<byte> signature, byte[]? devKey)
    {
        ArgumentNullException.ThrowIfNull(sha256Hex);
        if (signature.Length != 64)
        {
            return HookSigner.None;
        }
        var line = Encoding.ASCII.GetBytes(SignPrefix + sha256Hex.ToLowerInvariant());
        if (VerifyStrict(ReleaseKey.ToArray(), signature.ToArray(), line))
        {
            return HookSigner.Official;
        }
        return devKey is { Length: 32 } && VerifyStrict(devKey, signature.ToArray(), line) ? HookSigner.Development : HookSigner.None;
    }

    /// <summary>Every check of a hook's bytes and signature; the signer, or a <see cref="HookException"/>.</summary>
    public static (string Version, string Sha256, HookSigner Signer) Verify(byte[] bytes, byte[] signature, byte[]? devKey)
    {
        ArgumentNullException.ThrowIfNull(bytes);
        ArgumentNullException.ThrowIfNull(signature);
        if (bytes.Length > MaxHookBytes)
        {
            throw new HookException($"the hook is {bytes.Length} bytes (limit {MaxHookBytes})");
        }
        var sha = Convert.ToHexStringLower(SHA256.HashData(bytes));
        var signer = SignerOf(sha, signature, devKey);
        if (signer == HookSigner.None)
        {
            throw new HookException("the hook's signature does not verify against Infinity's release key");
        }
        if (bytes.Length < 2 || bytes[0] != 'M' || bytes[1] != 'Z')
        {
            throw new HookException("the hook is not a Windows DLL");
        }
        var version = VersionOf(bytes) ?? throw new HookException("the hook carries no +fh2online build");
        return (version, sha, signer);
    }

    /// <summary>The package next to the launcher (<paramref name="folder"/>: <c>&lt;launcher&gt;\hook</c>), verified; null when it is not there.</summary>
    public static HookPackage? LoadPackage(string folder, byte[]? devKey)
    {
        var dll = Path.Combine(folder, PackageFile);
        var sig = dll + ".sig";
        if (!File.Exists(dll) || !File.Exists(sig))
        {
            return null;
        }
        if (new FileInfo(dll).Length > MaxHookBytes)
        {
            throw new HookException("the hook package is too large");
        }
        var bytes = File.ReadAllBytes(dll);
        var signature = File.ReadAllBytes(sig);
        var (version, sha, signer) = Verify(bytes, signature, devKey);
        return new HookPackage(dll, bytes, signature, version, sha, signer);
    }

    /// <summary>Where <see cref="HookDll"/> goes: beside <c>azimuth.exe</c> (OD-125).</summary>
    private static string HookFolder(string azimuthRoot) => azimuthRoot;

    private static string KeptSignature(string azimuthRoot) => Path.Combine(azimuthRoot, "data", "fi-launcher", "hook", HookDll + ".sig");

    /// <summary>What <c>&lt;Azimuth&gt;\winmm.dll</c> is, and whether the signature the launcher kept signs exactly these bytes.</summary>
    public static InstalledHook Inspect(string azimuthRoot, byte[]? devKey)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        var path = Path.Combine(HookFolder(azimuthRoot), HookDll);
        if (!File.Exists(path))
        {
            return new InstalledHook(HookKind.Missing, null, null, HookSigner.None);
        }
        var info = new FileInfo(path);
        if (info.Length > MaxHookBytes)
        {
            return new InstalledHook(HookKind.Unknown, null, null, HookSigner.None);
        }
        var bytes = File.ReadAllBytes(path);
        var sha = Convert.ToHexStringLower(SHA256.HashData(bytes));
        var version = VersionOf(bytes);
        if (version is null)
        {
            // Azimuth's helper only by its pinned hash; a marker without a readable version is ours;
            // anything else (another mod, a damaged file) is unknown and is never copied over .stock
            var kind = sha == Stock0910Sha256 ? HookKind.Stock
                : bytes.AsSpan().IndexOf("+fh2online."u8) >= 0 ? HookKind.Ours
                : HookKind.Unknown;
            return new InstalledHook(kind, null, sha, HookSigner.None);
        }
        var kept = KeptSignature(azimuthRoot);
        var signer = File.Exists(kept) && new FileInfo(kept).Length == 64 ? SignerOf(sha, File.ReadAllBytes(kept), devKey) : HookSigner.None;
        return new InstalledHook(HookKind.Ours, version, sha, signer);
    }

    /// <summary>
    /// Install or update to the package: when the installed one is not a verified Infinity hook,
    /// or the package is a newer build (never a downgrade of a verified hook).
    /// </summary>
    public static bool ShouldInstall(InstalledHook installed, HookPackage? package)
    {
        ArgumentNullException.ThrowIfNull(installed);
        if (package is null || installed.Sha256 == package.Sha256 && installed.Verified)
        {
            return false;
        }
        // never a lower build than an Infinity hook already there, verified or not (a newer
        // official build is reinstalled from a newer package, not replaced by an older one)
        if (installed.Kind == HookKind.Ours && Build(installed.Version) is { } have && Build(package.Version) is { } offer && offer < have)
        {
            return false;
        }
        return !installed.Verified || Build(package.Version) > Build(installed.Version);
    }

    /// <summary>Explicit install/repair also permits the current signed build, while still refusing downgrades.</summary>
    public static bool CanInstallManually(InstalledHook installed, HookPackage? package)
    {
        ArgumentNullException.ThrowIfNull(installed);
        return ShouldInstall(installed, package) ||
            (package is not null && installed.Verified && Build(package.Version) is { } offer &&
             Build(installed.Version) is { } have && offer >= have);
    }

    /// <summary>
    /// Installs a verified package beside <c>azimuth.exe</c> (<see cref="HookFolder"/>). Refuses while <paramref name="gameRunning"/>
    /// or while the DLL is in use. The bytes are re-verified, written to <c>.new</c> (flushed, re-hashed),
    /// the current DLL renamed to <c>.previous</c> (Azimuth's own also copied to <c>.stock</c>),
    /// then <c>.new</c> renamed into place; a failed last step puts <c>.previous</c> back.
    /// </summary>
    public static InstalledHook Install(string azimuthRoot, HookPackage package, byte[]? devKey, Func<bool> gameRunning)
    {
        ArgumentNullException.ThrowIfNull(azimuthRoot);
        ArgumentNullException.ThrowIfNull(package);
        ArgumentNullException.ThrowIfNull(gameRunning);
        var (version, sha, _) = Verify(package.Bytes, package.Signature, devKey);
        var bin = HookFolder(azimuthRoot);
        var target = Path.Combine(bin, HookDll);
        if (gameRunning() || InUse(target))
        {
            throw new HookException("close the game first");
        }
        Directory.CreateDirectory(bin);
        var previous = Inspect(azimuthRoot, devKey);
        if (previous.Kind == HookKind.Ours && Build(previous.Version) is { } have && Build(version) is { } offer && offer < have)
        {
            throw new HookException("a newer Infinity hook is already installed");
        }
        if (previous.Kind == HookKind.Stock)
        {
            // first install, or an Azimuth update put its own DLL back: keep the newest stock copy
            File.Copy(target, Path.Combine(bin, HookDll + ".stock"), overwrite: true);
        }
        else if (previous.Kind == HookKind.Unknown && previous.Sha256 is { } foreign)
        {
            // another mod's DLL: kept once under its own name, never overwritten later
            var keep = Path.Combine(bin, $"{HookDll}.foreign-{foreign[..8]}");
            if (!File.Exists(keep))
            {
                File.Copy(target, keep);
            }
        }
        var fresh = target + ".new";
        using (var stream = new FileStream(fresh, FileMode.Create, FileAccess.Write, FileShare.None))
        {
            stream.Write(package.Bytes);
            stream.Flush(flushToDisk: true);
        }
        if (Convert.ToHexStringLower(SHA256.HashData(File.ReadAllBytes(fresh))) != sha)
        {
            File.Delete(fresh);
            throw new HookException("the written hook does not hash to the package");
        }
        // the new signature is staged before the swap; every failure after it puts both back
        var kept = KeptSignature(azimuthRoot);
        Directory.CreateDirectory(Path.GetDirectoryName(kept)!);
        var oldSignature = File.Exists(kept) ? File.ReadAllBytes(kept) : null;
        File.WriteAllBytes(kept + ".tmp", package.Signature);
        var backup = target + ".previous";
        var had = previous.Kind != HookKind.Missing;
        if (had)
        {
            File.Move(target, backup, overwrite: true);
        }
        try
        {
            File.Move(fresh, target);
            File.Move(kept + ".tmp", kept, overwrite: true);
            var now = Inspect(azimuthRoot, devKey);
            if (!now.Verified || now.Sha256 != sha || now.Version != version)
            {
                throw new HookException("the installed hook did not verify");
            }
            return now;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or HookException)
        {
            RollBack(target, backup, had, fresh, kept, oldSignature);
            throw;
        }
    }

    /// <summary>Puts the previous DLL and its kept signature back after a failed swap (best effort, each step on its own).</summary>
    private static void RollBack(string target, string backup, bool had, string fresh, string kept, byte[]? oldSignature)
    {
        void Try(Action step)
        {
            try
            {
                step();
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException)
            {
                // the next step still runs; the screen reads the folder again afterwards
            }
        }
        if (had)
        {
            Try(() => File.Move(backup, target, overwrite: true));
        }
        else
        {
            Try(() => File.Delete(target));
        }
        Try(() => File.Delete(fresh));
        Try(() => File.Delete(kept + ".tmp"));
        Try(() =>
        {
            if (oldSignature is null)
            {
                File.Delete(kept);
            }
            else
            {
                File.WriteAllBytes(kept, oldSignature);
            }
        });
    }

    /// <summary>Whether another process holds the DLL (a running game maps it).</summary>
    private static bool InUse(string path)
    {
        if (!File.Exists(path))
        {
            return false;
        }
        try
        {
            using var _ = new FileStream(path, FileMode.Open, FileAccess.ReadWrite, FileShare.None);
            return false;
        }
        catch (IOException)
        {
            return true;
        }
    }

    /// <summary>Ed25519 as ed25519-dalek's <c>verify_strict</c> (the same check as the launch ticket's).</summary>
    private static bool VerifyStrict(byte[] key, byte[] signature, byte[] message) =>
        Ed25519.ValidatePublicKeyFull(key, 0) && Ed25519.Verify(signature, 0, key, 0, message, 0, message.Length);
}
