// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using Infinity.Core.Api;

namespace Infinity.Core.Auth;

/// <summary>
/// Stay signed in (OD-105: automatic, no "keep me signed in" choice): the platform's
/// device-bound refresh token, encrypted for this Windows user with DPAPI, in
/// <c>&lt;state&gt;\session.bin</c>. Saved on every sign-in and every rotation (before the new token
/// is used again), deleted on sign-out and when the server refuses it. Never the password or an
/// Identity Platform token. Nothing is kept where DPAPI is unavailable.
/// </summary>
public static class SessionStore
{
    public const string FileName = "session.bin";

    public sealed record Saved(string AccountId, string? Email, Secret RefreshToken);

    private sealed record Stored(string AccountId, string? Email, string RefreshToken);

    private static readonly byte[] Entropy = Encoding.UTF8.GetBytes("infinity-horizon-session-v1");

    public static void Save(string stateDirectory, string accountId, string? email, Secret refreshToken)
    {
        ArgumentNullException.ThrowIfNull(refreshToken);
        if (!OperatingSystem.IsWindows()) return;
        var plain = JsonSerializer.SerializeToUtf8Bytes(new Stored(accountId, email, refreshToken.Expose()), ApiClient.Json);
        try
        {
            var sealedBytes = Protect(plain, encrypt: true);
            Directory.CreateDirectory(stateDirectory);
            var path = Path.Combine(stateDirectory, FileName);
            File.WriteAllBytes(path + ".tmp", sealedBytes);
            File.Move(path + ".tmp", path, overwrite: true);
        }
        finally
        {
            Array.Clear(plain);
        }
    }

    public static Saved? Load(string stateDirectory)
    {
        if (!OperatingSystem.IsWindows()) return null;
        byte[]? plain = null;
        try
        {
            var path = Path.Combine(stateDirectory, FileName);
            if (!File.Exists(path) || new FileInfo(path).Length > 65536) return null;
            plain = Protect(File.ReadAllBytes(path), encrypt: false);
            var stored = JsonSerializer.Deserialize<Stored>(plain, ApiClient.Json);
            return stored is { AccountId.Length: > 0, RefreshToken.Length: > 0 }
                ? new Saved(stored.AccountId, stored.Email, new Secret(stored.RefreshToken)) : null;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException or System.ComponentModel.Win32Exception)
        {
            return null;
        }
        finally
        {
            if (plain is not null) Array.Clear(plain);
        }
    }

    /// <summary>A session is kept (not yet proven): the shell waits for it instead of offering sign-in.</summary>
    public static bool Exists(string stateDirectory) => OperatingSystem.IsWindows() && File.Exists(Path.Combine(stateDirectory, FileName));

    public static void Delete(string stateDirectory)
    {
        try
        {
            File.Delete(Path.Combine(stateDirectory, FileName));
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            // A stale token is refused by the server on the next start, and then deleted again.
        }
    }

    private static byte[] Protect(byte[] data, bool encrypt)
    {
        var input = Blob.Of(data);
        var entropy = Blob.Of(Entropy);
        try
        {
            var output = default(Blob);
            var ok = encrypt
                ? CryptProtectData(ref input, null, ref entropy, IntPtr.Zero, IntPtr.Zero, UiForbidden, ref output)
                : CryptUnprotectData(ref input, IntPtr.Zero, ref entropy, IntPtr.Zero, IntPtr.Zero, UiForbidden, ref output);
            if (!ok) throw new System.ComponentModel.Win32Exception(Marshal.GetLastWin32Error());
            try
            {
                var result = new byte[output.Size];
                Marshal.Copy(output.Data, result, 0, result.Length);
                return result;
            }
            finally
            {
                LocalFree(output.Data);
            }
        }
        finally
        {
            Marshal.FreeHGlobal(input.Data);
            Marshal.FreeHGlobal(entropy.Data);
        }
    }

    private const int UiForbidden = 0x1;

    [StructLayout(LayoutKind.Sequential)]
    private struct Blob
    {
        public int Size;
        public IntPtr Data;

        public static Blob Of(byte[] bytes)
        {
            var blob = new Blob { Size = bytes.Length, Data = Marshal.AllocHGlobal(Math.Max(bytes.Length, 1)) };
            Marshal.Copy(bytes, 0, blob.Data, bytes.Length);
            return blob;
        }
    }

    [DllImport("crypt32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool CryptProtectData(ref Blob dataIn, string? description, ref Blob entropy, IntPtr reserved, IntPtr prompt, int flags, ref Blob dataOut);

    [DllImport("crypt32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool CryptUnprotectData(ref Blob dataIn, IntPtr description, ref Blob entropy, IntPtr reserved, IntPtr prompt, int flags, ref Blob dataOut);

    [DllImport("kernel32.dll")]
    private static extern IntPtr LocalFree(IntPtr memory);
}
