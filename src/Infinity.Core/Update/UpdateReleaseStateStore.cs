// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Globalization;
using System.Text.Json;
using System.Text.Json.Serialization;
using Infinity.Core.Api;

namespace Infinity.Core.Update;

/// <summary>Local verified release high-water mark. Reading never creates or renews release authority.</summary>
internal static class UpdateReleaseStateStore
{
    private const int MaxBytes = 65_536;
    private const string StateName = "launcher-release-state.json";
    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web)
    {
        PropertyNameCaseInsensitive = false,
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
        RespectRequiredConstructorParameters = true,
        MaxDepth = 12,
    };

    internal static void ValidateKnown(string stateDirectory, LauncherReleaseDescriptor manifest, byte[] raw)
    {
        ArgumentNullException.ThrowIfNull(manifest);
        ArgumentNullException.ThrowIfNull(raw);
        if (raw.Length is < 1 or > MaxBytes) throw new InvalidDataException("Invalid release history input.");
        var path = StatePath(stateDirectory);
        if (!File.Exists(path)) return;
        UpdateInstaller.RequireNoReparse(path);
        using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read | FileShare.Delete);
        if (stream.Length is < 1 or > MaxBytes) throw new InvalidDataException("Invalid release history size.");
        var previousBytes = new byte[(int)stream.Length];
        stream.ReadExactly(previousBytes);
        if (stream.ReadByte() != -1) throw new InvalidDataException("Release history changed while reading.");
        LauncherReleaseDescriptor previous;
        try
        {
            var reader = new Utf8JsonReader(previousBytes, new JsonReaderOptions { MaxDepth = 12 });
            var names = new Stack<HashSet<string>>();
            while (reader.Read())
            {
                if (reader.TokenType == JsonTokenType.StartObject) names.Push(new(StringComparer.Ordinal));
                else if (reader.TokenType == JsonTokenType.EndObject) names.Pop();
                else if (reader.TokenType == JsonTokenType.PropertyName && !names.Peek().Add(reader.GetString()!))
                    throw new InvalidDataException("Duplicate release history property.");
            }
            previous = JsonSerializer.Deserialize<LauncherReleaseDescriptor>(previousBytes, Json)
                ?? throw new InvalidDataException("Invalid release history.");
        }
        catch (JsonException error) { throw new InvalidDataException("Invalid release history.", error); }
        var priorVersion = ValidateShape(previous);
        var version = ValidateShape(manifest);
        if (manifest.ReleaseSequence < previous.ReleaseSequence || version < priorVersion)
            throw new InvalidDataException("Release rollback is forbidden.");
        if (manifest.ReleaseSequence == previous.ReleaseSequence)
        {
            if (!CryptographicOperations.FixedTimeEquals(SHA256.HashData(raw), SHA256.HashData(previousBytes)))
                throw new InvalidDataException("A release sequence cannot change its descriptor.");
            return;
        }
        if (version == priorVersion && (manifest.Version != previous.Version || manifest.SourceCommit != previous.SourceCommit
            || manifest.Package != previous.Package || !SameFiles(manifest, previous)))
            throw new InvalidDataException("A metadata refresh cannot change runtime contents.");
    }

    internal static void RecordKnown(string stateDirectory, LauncherReleaseDescriptor manifest, byte[] raw)
    {
        var path = StatePath(stateDirectory);
        UpdateInstaller.RequireNoReparse(stateDirectory);
        Directory.CreateDirectory(stateDirectory);
        UpdateInstaller.RequireNoReparse(stateDirectory);
        var lockPath = path + ".lock";
        UpdateInstaller.RequireNoReparse(lockPath);
        using var exclusive = new FileStream(lockPath, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
        ValidateKnown(stateDirectory, manifest, raw);
        var temporary = path + "." + Guid.NewGuid().ToString("N") + ".tmp";
        try
        {
            using (var stream = new FileStream(temporary, FileMode.CreateNew, FileAccess.Write, FileShare.None,
                4096, FileOptions.WriteThrough))
            {
                stream.Write(raw);
                stream.Flush(flushToDisk: true);
            }
            UpdateInstaller.RequireNoReparse(path);
            File.Move(temporary, path, overwrite: true);
        }
        finally { if (File.Exists(temporary)) File.Delete(temporary); }
    }

    private static bool SameFiles(LauncherReleaseDescriptor left, LauncherReleaseDescriptor right)
        => left.Files.Count == right.Files.Count && left.Files.OrderBy(f => f.Path, StringComparer.Ordinal)
            .SequenceEqual(right.Files.OrderBy(f => f.Path, StringComparer.Ordinal));

    private static LauncherVersion ValidateShape(LauncherReleaseDescriptor value)
    {
        if (value.SchemaVersion != 1 || value.Product != "infinity-horizon" || value.Repository != GitHubReleases.Repository
            || value.Channel != "stable" || value.Platform != "windows-x64" || !LauncherVersion.TryParse(value.Version, out var version)
            || version.Prerelease is not null || value.Tag != "v" + value.Version || value.ReleaseSequence is < 1 or > 9_007_199_254_740_991
            || !Hex(value.SourceCommit, 40) || !Instant(value.IssuedAt, out var issued) || !Instant(value.ExpiresAt, out var expires)
            || expires <= issued || expires - issued > TimeSpan.FromDays(180) || value.Package is not { } package
            || package.Name != $"infinity-horizon-{value.Version}-windows-x64.zip" || package.ByteLength is < 1 or > 536_870_912
            || !Hex(package.Sha256, 64) || value.Files is not { Count: > 0 and <= 128 })
            throw new InvalidDataException("Invalid release history fields.");
        var paths = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        ulong total = 0;
        foreach (var file in value.Files)
        {
            if (file is null || !UpdateInstaller.IsRuntimePath(file.Path) || !paths.Add(file.Path) || !Hex(file.Sha256, 64)
                || file.ByteLength > 1_073_741_824 || total > 1_073_741_824 - file.ByteLength)
                throw new InvalidDataException("Invalid release history inventory.");
            total += file.ByteLength;
        }
        return version;
    }
    private static bool Hex(string? value, int length) => value?.Length == length && value.All(c => c is >= '0' and <= '9' or >= 'a' and <= 'f');
    private static bool Instant(string? value, out DateTimeOffset time) => DateTimeOffset.TryParseExact(value, "yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'",
        CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out time);
    private static string StatePath(string stateDirectory)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(stateDirectory);
        return Path.Combine(Path.GetFullPath(stateDirectory), StateName);
    }
}
