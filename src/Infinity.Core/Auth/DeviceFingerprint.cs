// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text;

namespace Infinity.Core.Auth;

/// <summary>
/// OD-120 (CR-S44, owner answer to OD120-Q1): this PC's anonymous same-device value, <c>Device.fingerprint</c>. It is
/// base64url without padding of SHA-256 over <c>"forzainfinity-device-v1:" + lower-case MachineGuid</c>: 43 characters, one per
/// Windows install (one per Wine prefix under Proton). The raw MachineGuid never leaves this class. The value lives in memory
/// only: it is never written to <c>device.json</c>, logs or crash reports, and the server keeps only an HMAC of it.
/// </summary>
public static class DeviceFingerprint
{
    public const string Salt = "forzainfinity-device-v1:";

    /// <summary>The fingerprint of <paramref name="machineGuid"/>; null when it is missing or not a GUID (the field is then left out).</summary>
    public static string? From(string? machineGuid)
    {
        if (!Guid.TryParseExact(machineGuid?.Trim(), "D", out var guid)) return null;
        var digest = SHA256.HashData(Encoding.UTF8.GetBytes(Salt + guid.ToString("D")));
        return Convert.ToBase64String(digest).TrimEnd('=').Replace('+', '-').Replace('/', '_');
    }

    /// <summary>
    /// This PC's fingerprint: <c>HKLM\SOFTWARE\Microsoft\Cryptography\MachineGuid</c> from the 64-bit registry view (the Wine
    /// prefix's own value under Proton). Null when it cannot be read; a sign-in then simply goes without it.
    /// </summary>
    public static string? Read()
    {
        if (!OperatingSystem.IsWindows()) return null;
        try
        {
            using var hive = Microsoft.Win32.RegistryKey.OpenBaseKey(Microsoft.Win32.RegistryHive.LocalMachine, Microsoft.Win32.RegistryView.Registry64);
            using var key = hive.OpenSubKey(@"SOFTWARE\Microsoft\Cryptography");
            return From(key?.GetValue("MachineGuid") as string);
        }
        catch (Exception error) when (error is System.Security.SecurityException or UnauthorizedAccessException or IOException) { return null; }
    }
}
