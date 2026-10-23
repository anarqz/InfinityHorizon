// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;
using Org.BouncyCastle.Math.EC.Rfc8032;

var path = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "ForzaInfinity", "dev-hook-seed.bin");
Directory.CreateDirectory(Path.GetDirectoryName(path)!);
if (!File.Exists(path))
{
    using var file = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.None);
    file.Write(RandomNumberGenerator.GetBytes(32));
}
var seed = File.ReadAllBytes(path);
if (seed.Length != 32) throw new InvalidDataException("Invalid local development key.");
var key = new byte[32];
Ed25519.GeneratePublicKey(seed, 0, key, 0);
switch (args)
{
    case ["key"]:
        Console.WriteLine(Convert.ToHexStringLower(key));
        return 0;
    case ["sign", var file]:
        return Sign(file, "fh2online-update-v1 ");
    case ["sign-mod", var file]:
        // the native mod runtime and helper: their own domain (the hook's mod_host.c verifies it)
        return Sign(file, "fi-mod-runtime-v1 ");
    default:
        Console.Error.WriteLine("usage: devsign key | sign <file> | sign-mod <file>");
        return 2;
}

int Sign(string file, string prefix)
{
    var line = Encoding.ASCII.GetBytes(prefix + Convert.ToHexStringLower(SHA256.HashData(File.ReadAllBytes(file))));
    var signature = new byte[64];
    Ed25519.Sign(seed, 0, line, 0, line.Length, signature, 0);
    if (!Ed25519.Verify(signature, 0, key, 0, line, 0, line.Length))
        throw new CryptographicException("Development signature verification failed.");
    File.WriteAllBytes(file + ".sig", signature);
    return 0;
}
