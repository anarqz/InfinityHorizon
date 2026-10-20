// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;
using Org.BouncyCastle.Math.EC.Rfc8032;

namespace Infinity.Core.Process;

/// <summary>Detached release signatures cover the exact on-disk file, including a single-file launcher.</summary>
public static class ReleaseSignature
{
    public const long MaxFileBytes = 512L * 1024 * 1024;

    public static bool IsOfficialFile(string? path, string? signaturePath = null) =>
        path is not null && VerifyFile(path, signaturePath ?? path + ".sig", HookInstaller.ReleaseKey);

    public static bool VerifyFile(string path, string signaturePath, ReadOnlySpan<byte> publicKey)
    {
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
            var message = Encoding.ASCII.GetBytes(HookInstaller.SignPrefix + Convert.ToHexStringLower(digest));
            return Ed25519.Verify(signature, 0, publicKey.ToArray(), 0, message, 0, message.Length);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            return false;
        }
    }
}
