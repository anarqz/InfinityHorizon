// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;
using Org.BouncyCastle.Math.EC.Rfc8032;

namespace Infinity.Core.Process;

/// <summary>Detached release signatures cover the exact on-disk file, including a single-file launcher.</summary>
public static class ReleaseSignature
{
    public const long MaxFileBytes = 512L * 1024 * 1024;

    /// <summary>
    /// The native mod runtime and its helper (<c>mod/</c>): <c>"fi-mod-runtime-v1 " + SHA-256 lowercase hex</c>,
    /// the line the hook verifies before it loads the runtime (native/hook mod_host.c, sign.h). Its own domain, so a
    /// runtime signature never stands for a hook update and the other way round.
    /// </summary>
    public const string ModRuntimePrefix = "fi-mod-runtime-v1 ";

    /// <summary>
    /// The line a release binary's <c>.sig</c> signs: <c>"fh2online-update-v1 " + SHA-256 lowercase hex</c>, the hook's own
    /// update line (<c>HookInstaller.SignPrefix</c> is this constant).
    /// </summary>
    public const string SignPrefix = "fh2online-update-v1 ";

    /// <summary>
    /// The project's release public key, the same 32 bytes as <c>azimuth_sign_release_key</c> in sign.c
    /// (<c>HookInstaller.ReleaseKey</c> is this key). U2: here, so infinity-updater.exe carries it without the rest of Core.
    /// </summary>
    public static ReadOnlySpan<byte> ReleaseKey =>
    [
        0xbf, 0xe5, 0x72, 0xbc, 0x40, 0x36, 0x71, 0x2e, 0x5a, 0x2f, 0x96, 0x83, 0xc6, 0xb9, 0xfe, 0x29,
        0x3d, 0xce, 0x19, 0x62, 0x74, 0xe0, 0x8b, 0x10, 0x83, 0x0e, 0xf0, 0x23, 0x28, 0x60, 0xae, 0xfb,
    ];

    public static bool IsOfficialFile(string? path, string? signaturePath = null) =>
        path is not null && VerifyFile(path, signaturePath ?? path + ".sig", ReleaseKey);

    public static bool VerifyFile(string path, string signaturePath, ReadOnlySpan<byte> publicKey) =>
        VerifyFile(path, signaturePath, publicKey, SignPrefix);

    public static bool VerifyFile(string path, string signaturePath, ReadOnlySpan<byte> publicKey, string prefix)
    {
        ArgumentNullException.ThrowIfNull(prefix);
        if (publicKey.Length != 32 || !Ed25519.ValidatePublicKeyFull(publicKey.ToArray(), 0)) return false;
        try
        {
            using var signatureFile = File.OpenRead(signaturePath);
            if (signatureFile.Length != 64) return false;
            var signature = new byte[64];
            signatureFile.ReadExactly(signature);
            using var file = File.OpenRead(path);
            if (file.Length is <= 0 or > MaxFileBytes) return false;
            var digest = SHA256.HashData(file);
            var message = Encoding.ASCII.GetBytes(prefix + Convert.ToHexStringLower(digest));
            return Ed25519.Verify(signature, 0, publicKey.ToArray(), 0, message, 0, message.Length);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            return false;
        }
    }
}
