// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Org.BouncyCastle.Math.EC.Rfc8032;

namespace Infinity.Core.Update;

/// <summary>Authenticates exact release bytes and the complete staged runtime, independently in launcher and helper.</summary>
public sealed class UpdatePackageVerifier : IUpdatePlanVerifier
{
    public const string DescriptorName = "infinity-horizon-update-windows-x64.json";
    public const string SignaturePrefix = "fi-launcher-release-v1\n";
    public const int MaxDescriptorBytes = 65536;
    public const long MaxZipBytes = 536870912;
    public const long MaxExpandedBytes = 1073741824;
    public static string DefaultStateDirectory => Auth.ServerConfig.Default.StateDirectory(Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "ForzaInfinity", "launcher"));
    private readonly byte[] _key;
    private readonly TimeProvider _clock;
    private readonly string? _stateDirectory;
    private static readonly string[] Required = ["infinity.exe", "infinity-updater.exe", "hook/infinity.dll", "overlay/infinity-overlay.dll",
        "infinity.exe.sig", "infinity-updater.exe.sig", "hook/infinity.dll.sig", "overlay/infinity-overlay.dll.sig",
        "i18n/en.json", "i18n/pt-BR.json", "i18n-win/en.json", "i18n-win/pt-BR.json", "LICENSE", "NOTICE",
        "licenses/BouncyCastle-LICENSE.md", "licenses/fh2-mods-LICENSE.txt", "licenses/Hook-NOTICE.md", "licenses/ImGui-LICENSE.txt",
        "licenses/MinHook-LICENSE.txt", "licenses/Selawik-LICENSE.txt", "licenses/FluentSystemIcons-LICENSE.txt", "SHA256SUMS", "SHA256SUMS.sig"];
    public static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web)
    {
        PropertyNameCaseInsensitive = false, NumberHandling = JsonNumberHandling.Strict,
        RespectRequiredConstructorParameters = true, UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow, MaxDepth = 12,
    };

    public UpdatePackageVerifier() : this(HookInstaller.ReleaseKey.ToArray(), TimeProvider.System, DefaultStateDirectory) { }
    internal UpdatePackageVerifier(byte[] key, TimeProvider clock, string? stateDirectory = null)
    {
        _key = key.ToArray(); _clock = clock; _stateDirectory = stateDirectory;
        if (_key.Length != 32 || !Ed25519.ValidatePublicKeyFull(_key, 0)) throw new ArgumentException("Invalid release key", nameof(key));
    }

    public static byte[] SignatureMessage(ReadOnlySpan<byte> raw) => Encoding.ASCII.GetBytes(SignaturePrefix + Convert.ToHexStringLower(SHA256.HashData(raw)));
    public LauncherReleaseDescriptor VerifyDescriptor(byte[] raw, byte[] signature) => Parse(raw, signature, active: true);
    public LauncherReleaseDescriptor VerifyPlanDescriptor(UpdateApplyPlan plan)
    {
        ArgumentNullException.ThrowIfNull(plan);
        var transaction = Path.GetDirectoryName(Path.GetFullPath(plan.StageRoot))!;
        return VerifyDescriptor(ReadBounded(Metadata(plan.DescriptorPath, transaction), MaxDescriptorBytes),
            ReadBounded(Metadata(plan.DescriptorSignaturePath, transaction), 64));
    }

    private LauncherReleaseDescriptor Parse(byte[] raw, byte[] signature, bool active)
    {
        if (raw.Length is <= 0 or > MaxDescriptorBytes || signature.Length != 64 || raw.AsSpan().StartsWith(new byte[] { 239, 187, 191 })) Invalid();
        var message = SignatureMessage(raw);
        if (!Ed25519.Verify(signature, 0, _key, 0, message, 0, message.Length)) Invalid();
        // Reject duplicate names including escaped aliases before exact field/type deserialization.
        var reader = new Utf8JsonReader(raw, new JsonReaderOptions { MaxDepth = 12 });
        var names = new Stack<HashSet<string>>();
        while (reader.Read())
        {
            if (reader.TokenType == JsonTokenType.StartObject) names.Push(new(StringComparer.Ordinal));
            else if (reader.TokenType == JsonTokenType.EndObject) names.Pop();
            else if (reader.TokenType == JsonTokenType.PropertyName && !names.Peek().Add(reader.GetString()!)) Invalid();
        }
        var value = JsonSerializer.Deserialize<LauncherReleaseDescriptor>(raw, Json) ?? throw new InvalidDataException("Missing release descriptor");
        if (value.SchemaVersion != 1 || value.Product != "infinity-horizon" || value.Repository != GitHubReleases.Repository
            || value.Channel != "stable" || value.Platform != "windows-x64" || !LauncherVersion.TryParse(value.Version, out var version)
            || version.Prerelease is not null || value.Tag != "v" + value.Version || !Hex(value.SourceCommit, 40)
            || value.ReleaseSequence is 0 or > 9007199254740991 || !Instant(value.IssuedAt, out var issued) || !Instant(value.ExpiresAt, out var expires)
            || expires <= issued || expires - issued > TimeSpan.FromDays(180)
            || active && (issued > _clock.GetUtcNow().AddSeconds(300) || expires <= _clock.GetUtcNow())
            || value.Package is not { } package || package.Name != $"infinity-horizon-{value.Version}-windows-x64.zip"
            || package.ByteLength is 0 or > MaxZipBytes || !Hex(package.Sha256, 64) || value.Files is not { Count: > 0 and <= 128 }) Invalid();
        var paths = new HashSet<string>(StringComparer.OrdinalIgnoreCase); ulong total = 0;
        foreach (var file in value.Files)
        {
            if (file is null || !UpdateInstaller.IsRuntimePath(file.Path) || !paths.Add(file.Path) || !Hex(file.Sha256, 64)
                || file.ByteLength > MaxExpandedBytes || total > (ulong)MaxExpandedBytes - file.ByteLength) Invalid();
            total += file.ByteLength;
        }
        if (Required.Any(path => !paths.Contains(path)) || value.Files.Any(file => file.Path.EndsWith(".sig", StringComparison.Ordinal) && file.ByteLength != 64)) Invalid();
        return value;
    }

    public async Task VerifyAsync(UpdateApplyPlan plan, CancellationToken cancellation)
    {
        ArgumentNullException.ThrowIfNull(plan);
        var transaction = Path.GetDirectoryName(Path.GetFullPath(plan.StageRoot))!;
        var raw = ReadBounded(Metadata(plan.DescriptorPath, transaction), MaxDescriptorBytes);
        var descriptor = VerifyDescriptor(raw, ReadBounded(Metadata(plan.DescriptorSignaturePath, transaction), 64));
        if (_stateDirectory is not null) UpdateReleaseStateStore.ValidateKnown(_stateDirectory, descriptor, raw);
        if (!SameFiles(plan.Files, descriptor.Files)) Invalid();
        var previous = Previous(plan, transaction);
        if (!SamePrevious(plan.PreviousFiles, previous)) Invalid();
        foreach (var file in plan.Files) await CheckFileAsync(Path.Combine(plan.StageRoot, file.Path.Replace('/', Path.DirectorySeparatorChar)), file, cancellation).ConfigureAwait(false);
        foreach (var binary in new[] { "infinity.exe", "infinity-updater.exe", "hook/infinity.dll", "overlay/infinity-overlay.dll", "SHA256SUMS" })
            if (!ReleaseSignature.VerifyFile(Path.Combine(plan.StageRoot, binary), Path.Combine(plan.StageRoot, binary + ".sig"), _key)) Invalid();
        foreach (var retired in previous.Where(old => !plan.Files.Any(file => file.Path == old.Path)))
        {
            var path = Path.Combine(plan.InstallRoot, retired.Path.Replace('/', Path.DirectorySeparatorChar));
            if (File.Exists(path)) await CheckFileAsync(path, retired, cancellation).ConfigureAwait(false);
        }
    }

    public UpdateApplyPlan WithPreviousInventory(UpdateApplyPlan plan)
    {
        var transaction = Path.GetDirectoryName(plan.StageRoot)!;
        var store = Path.Combine(plan.InstallRoot, ".infinity-update");
        LauncherReleaseDescriptor? last = null; UpdateApplyPlan? prior = null;
        UpdateInstaller.RequireNoReparse(store);
        if (Directory.Exists(store))
            foreach (var directory in Directory.EnumerateDirectories(store).Take(UpdateInstaller.MaxTransactions + 1))
            {
                var journal = Path.Combine(directory, "journal.json");
                if (!File.Exists(journal)) continue;
                var saved = UpdateJournalStore.Read<UpdateJournal>(journal);
                if (saved.Phase != "committed" || !Guid.TryParseExact(Path.GetFileName(directory), "N", out _)) continue;
                var candidate = UpdateInstaller.ReadPlan(Path.Combine(directory, "plan.json"));
                var metadata = Metadata(candidate.DescriptorPath, directory);
                var descriptor = Parse(ReadBounded(metadata, MaxDescriptorBytes), ReadBounded(Metadata(candidate.DescriptorSignaturePath, directory), 64), active: false);
                if (last is null || descriptor.ReleaseSequence > last.ReleaseSequence) { last = descriptor; prior = candidate; }
            }
        if (prior is not null && last is not null)
        {
            var descriptor = CopyMetadata(prior.DescriptorPath!, transaction, "previous-release.json", MaxDescriptorBytes);
            var signature = CopyMetadata(prior.DescriptorSignaturePath!, transaction, "previous-release.json.sig", 64);
            return plan with { PreviousFiles = last.Files.Select(file => new UpdateFile(file.Path, (long)file.ByteLength, file.Sha256)).ToArray(),
                PreviousDescriptorPath = descriptor, PreviousDescriptorSignaturePath = signature };
        }
        var checksums = Path.Combine(plan.InstallRoot, "SHA256SUMS");
        if (File.Exists(checksums) && ReleaseSignature.VerifyFile(checksums, checksums + ".sig", _key))
        {
            var manifest = CopyMetadata(checksums, transaction, "previous-checksums.txt", MaxDescriptorBytes);
            var signature = CopyMetadata(checksums + ".sig", transaction, "previous-checksums.txt.sig", 64);
            var previous = LegacyFiles(manifest, signature);
            return plan with { PreviousFiles = previous, PreviousChecksumsPath = manifest, PreviousChecksumsSignaturePath = signature };
        }
        return plan;
    }

    private UpdateFile[] Previous(UpdateApplyPlan plan, string transaction)
    {
        if (plan.PreviousDescriptorPath is not null)
        {
            if (plan.PreviousChecksumsPath is not null) Invalid();
            var descriptor = Parse(ReadBounded(Metadata(plan.PreviousDescriptorPath, transaction), MaxDescriptorBytes),
                ReadBounded(Metadata(plan.PreviousDescriptorSignaturePath, transaction), 64), active: false);
            return descriptor.Files.Select(file => new UpdateFile(file.Path, (long)file.ByteLength, file.Sha256)).ToArray();
        }
        if (plan.PreviousChecksumsPath is not null) return LegacyFiles(Metadata(plan.PreviousChecksumsPath, transaction), Metadata(plan.PreviousChecksumsSignaturePath, transaction));
        if (plan.PreviousDescriptorSignaturePath is not null || plan.PreviousChecksumsSignaturePath is not null) Invalid();
        return [];
    }

    private UpdateFile[] LegacyFiles(string path, string signature)
    {
        if (!ReleaseSignature.VerifyFile(path, signature, _key)) Invalid();
        var lines = new UTF8Encoding(false, true).GetString(ReadBounded(path, MaxDescriptorBytes)).Split('\n', StringSplitOptions.RemoveEmptyEntries);
        if (lines.Length is 0 or > 128) Invalid();
        var files = new List<UpdateFile>(); var paths = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var raw in lines)
        {
            var line = raw.TrimEnd('\r');
            if (line.Length < 67 || line[64..66] != "  " || !Hex(line[..64], 64) || !UpdateInstaller.IsRuntimePath(line[66..]) || !paths.Add(line[66..])) Invalid();
            // The legacy checksum format has no sizes; authenticate bytes in the installation now.
            var actual = Path.Combine(Path.GetDirectoryName(path)!, line[66..].Replace('/', Path.DirectorySeparatorChar));
            // When copied into a transaction, resolve its original installation rather than sibling metadata.
            var parent = new DirectoryInfo(Path.GetDirectoryName(path)!);
            if (parent.Parent?.Name == ".infinity-update") actual = Path.Combine(parent.Parent.Parent!.FullName, line[66..].Replace('/', Path.DirectorySeparatorChar));
            UpdateInstaller.RequireNoReparse(actual);
            if (!File.Exists(actual)) continue;
            using var input = new FileStream(actual, FileMode.Open, FileAccess.Read, FileShare.Read);
            if (input.Length > MaxExpandedBytes || Convert.ToHexStringLower(SHA256.HashData(input)) != line[..64]) Invalid();
            files.Add(new(line[66..], input.Length, line[..64]));
        }
        return files.ToArray();
    }

    private static async Task CheckFileAsync(string path, UpdateFile expected, CancellationToken cancellation)
    {
        UpdateInstaller.RequireNoReparse(path);
        await using var input = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read, 65536, FileOptions.Asynchronous | FileOptions.SequentialScan);
        if (input.Length != expected.Size || Convert.ToHexStringLower(await SHA256.HashDataAsync(input, cancellation).ConfigureAwait(false)) != expected.Sha256) Invalid();
    }
    private static string Metadata(string? path, string transaction)
    {
        if (path is null || !Path.GetFullPath(Path.GetDirectoryName(path)!).Equals(Path.GetFullPath(transaction), StringComparison.OrdinalIgnoreCase)) Invalid();
        UpdateInstaller.RequireNoReparse(path); return path;
    }
    internal static byte[] ReadBounded(string path, int maximum)
    {
        UpdateInstaller.RequireNoReparse(path);
        using var input = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
        if (input.Length is <= 0 || input.Length > maximum) Invalid();
        var bytes = new byte[(int)input.Length]; input.ReadExactly(bytes); if (input.ReadByte() != -1) Invalid(); return bytes;
    }
    private static string CopyMetadata(string source, string transaction, string name, int maximum)
    {
        var bytes = ReadBounded(source, maximum); var destination = Path.Combine(transaction, name);
        UpdateInstaller.RequireNoReparse(destination);
        using var output = new FileStream(destination, FileMode.CreateNew, FileAccess.Write, FileShare.None); output.Write(bytes); output.Flush(true); return destination;
    }
    private static bool SameFiles(IReadOnlyList<UpdateFile>? files, IReadOnlyList<LauncherReleaseFile> declared) => files?.Count == declared.Count
        && declared.All(file => files.Any(item => item is not null && item.Path == file.Path && item.Size == (long)file.ByteLength && item.Sha256 == file.Sha256));
    private static bool SamePrevious(IReadOnlyList<UpdateFile>? files, UpdateFile[] declared) => files?.Count == declared.Length
        && declared.All(file => files.Any(item => item is not null && item == file));
    private static bool Hex(string? value, int length) => value?.Length == length && value.All(c => c is >= '0' and <= '9' or >= 'a' and <= 'f');
    private static bool Instant(string? value, out DateTimeOffset time) => DateTimeOffset.TryParseExact(value, "yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'",
        CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal, out time);
    [System.Diagnostics.CodeAnalysis.DoesNotReturn]
    private static void Invalid() => throw new InvalidDataException("Release package could not be verified");
}
