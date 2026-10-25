// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;
using Infinity.Core.Api;

namespace Infinity.Core.Auth;

/// <summary>
/// This install's stable device id (UUIDv7 minted on first run, ACCOUNTS.md §5): kept in
/// <c>&lt;state&gt;\device.json</c> and reused for every sign-in, since an account may bind only two
/// new devices per 30 days (ADR 0304, <c>SHARING.TOO_MANY_RECENT_DEVICES</c>).
/// </summary>
public static class DeviceIdentity
{
    public const string FileName = "device.json";

    private sealed record Stored(string DeviceId);

    /// <summary>
    /// The device of this install: its stored id (minted once), the PC name, the platform and, OD-120, this PC's anonymous
    /// fingerprint (<see cref="DeviceFingerprint"/>, computed now and never stored). <paramref name="fingerprint"/> replaces the
    /// registry read (tests).
    /// </summary>
    public static Device Load(string stateDirectory, string platform, Func<string?>? fingerprint = null)
    {
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, FileName);
        string? id = null;
        try
        {
            id = JsonSerializer.Deserialize<Stored>(File.ReadAllText(path), ApiClient.Json)?.DeviceId;
        }
        catch (Exception e) when (e is IOException or JsonException)
        {
            // First run (or an unreadable file): mint a new id below.
        }
        if (!Guid.TryParse(id, out _))
        {
            id = Guid.CreateVersion7().ToString();
            var temp = path + ".tmp";
            File.WriteAllText(temp, JsonSerializer.Serialize(new Stored(id), ApiClient.Json));
            File.Move(temp, path, overwrite: true);
        }
        return new Device(id!, Environment.MachineName, platform, (fingerprint ?? DeviceFingerprint.Read)());
    }
}
