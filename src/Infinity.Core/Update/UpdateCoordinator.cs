// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.IO.Compression;
using System.Globalization;
using System.Security.Cryptography;
using Infinity.Core.Api;
using Infinity.Core.Process;

namespace Infinity.Core.Update;

/// <summary>Fresh authenticated release discovery and preparation; application remains an explicit helper operation.</summary>
public sealed class UpdateCoordinator : IDisposable
{
    public const string DescriptorAssetName = "infinity-horizon-update-windows-x64.json";
    public const string SignatureAssetName = DescriptorAssetName + ".sig";
    private const long MaxUnpackedBytes = 1L << 30;
    private readonly string _stateDirectory;
    private readonly GitHubReleases _source;
    private readonly UpdatePackageVerifier _verifier;
    private readonly TimeProvider _clock;
    private readonly SemaphoreSlim _operation = new(1, 1);
    private Discovery? _discovery;
    private Discovery? _requiredDiscovery;
    private bool _discoveryAttempted;
    private bool _releaseUnavailable;
    private bool _disposed;

    public UpdateCoordinator(string stateDirectory)
        : this(stateDirectory, new GitHubReleases(),
            new UpdatePackageVerifier(HookInstaller.ReleaseKey.ToArray(), TimeProvider.System, stateDirectory), TimeProvider.System) { }

    internal UpdateCoordinator(string stateDirectory, GitHubReleases source, UpdatePackageVerifier verifier, TimeProvider? clock = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(stateDirectory);
        _stateDirectory = Path.GetFullPath(stateDirectory);
        _source = source ?? throw new ArgumentNullException(nameof(source));
        _verifier = verifier ?? throw new ArgumentNullException(nameof(verifier));
        _clock = clock ?? TimeProvider.System;
    }

    public async Task DiscoverAsync(CancellationToken cancellation = default)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        await _operation.WaitAsync(cancellation).ConfigureAwait(false);
        var previous = _discovery;
        try
        {
            _discovery = null;
            _requiredDiscovery = null;
            _discoveryAttempted = false;
            _releaseUnavailable = false;
            var release = await _source.GetLatestAsync(cancellation).ConfigureAwait(false)
                ?? throw new InvalidDataException("No approved release is available.");
            var descriptorAsset = Asset(release, DescriptorAssetName);
            var signatureAsset = Asset(release, SignatureAssetName);
            if (descriptorAsset.Size is < 1 or > 65_536 || signatureAsset.Size != 64)
                throw new InvalidDataException("Invalid release metadata asset size.");
            var raw = await _source.DownloadBytesAsync(descriptorAsset, 65_536, cancellation).ConfigureAwait(false);
            var signature = await _source.DownloadBytesAsync(signatureAsset, 64, cancellation).ConfigureAwait(false);
            var descriptor = _verifier.VerifyDescriptor(raw, signature);
            if (descriptor.Version != release.Version.Value || descriptor.Tag != release.Tag)
                throw new InvalidDataException("The signed release does not match the selected upstream tag.");
            var package = Asset(release, descriptor.Package.Name);
            if (package.Size != checked((long)descriptor.Package.ByteLength) || package.Sha256 is { } digest && digest != descriptor.Package.Sha256)
                throw new InvalidDataException("The signed package does not match upstream metadata.");
            package = package with { Sha256 = descriptor.Package.Sha256 };
            UpdateReleaseStateStore.RecordKnown(_stateDirectory, descriptor, raw);
            _discovery = new(descriptor, raw, signature, package, _clock.GetTimestamp(), _clock.GetUtcNow());
        }
        catch
        {
            // A failed refresh cannot erase a still-valid, verified mandatory release.
            _discovery = Usable(previous) ? previous : null;
            _releaseUnavailable = true;
            throw;
        }
        finally { _discoveryAttempted = true; _operation.Release(); }
    }

    public UpdateDecision Evaluate(string current, ServerInfo info, string? game)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        ArgumentNullException.ThrowIfNull(info);
        var decision = EvaluateCore(current, info, game);
        _requiredDiscovery = decision.Phase == UpdatePhase.Required ? _discovery : null;
        return decision;
    }

    private UpdateDecision EvaluateCore(string current, ServerInfo info, string? game)
    {
        var discovery = _discovery;
        if (!LauncherVersion.TryParse(current, out var installed)
            || !LauncherVersion.TryParse(info.MinLauncher, out var minimum)
            || !LauncherVersion.TryParse(info.LatestLauncher, out var latest) || latest < minimum)
            return new(UpdatePhase.Unavailable, null);
        var floor = minimum;
        if (game is not null)
        {
            var selected = info.Games?.Where(item => item.Id == game).ToArray();
            if (selected is not { Length: 1 } || !LauncherVersion.TryParse(selected[0].MinLauncher, out var gameMinimum))
                return new(UpdatePhase.Unavailable, null);
            if (gameMinimum > floor) floor = gameMinimum;
        }
        if (!Usable(discovery))
        {
            if (!_discoveryAttempted) return new(UpdatePhase.Unavailable, null);
            // GitHub availability is advisory; a verified backend minimum remains authoritative.
            return installed >= floor ? new(UpdatePhase.ReleaseUnavailable, floor.Value) : new(UpdatePhase.Unavailable, floor.Value);
        }
        var target = latest > floor ? latest : floor;
        if (!LauncherVersion.TryParse(discovery!.Descriptor.Version, out var approved)) return new(UpdatePhase.Unavailable, null);
        if (_releaseUnavailable)
        {
            if (installed < approved && approved >= floor) return new(UpdatePhase.Required, approved.Value);
            return installed < floor ? new(UpdatePhase.Unavailable, floor.Value) : new(UpdatePhase.ReleaseUnavailable, floor.Value);
        }
        if (approved > target) target = approved;
        if (installed >= target) return new(UpdatePhase.Current, target.Value);
        return approved >= target ? new(UpdatePhase.Required, approved.Value) : new(UpdatePhase.Unavailable, target.Value);
    }

    public async Task<UpdateApplyPlan> StageAsync(string installRoot, IReadOnlyList<string> restartArgs,
        IProgress<UpdateProgress> progress, CancellationToken cancellation = default)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        ArgumentNullException.ThrowIfNull(restartArgs);
        ArgumentNullException.ThrowIfNull(progress);
        await _operation.WaitAsync(cancellation).ConfigureAwait(false);
        string? transactionId = null;
        try
        {
            var discovery = _discovery;
            if (!Usable(discovery) || !ReferenceEquals(discovery, _requiredDiscovery))
                throw new InvalidDataException("An evaluated required release is needed before staging.");
            // Repeat signature, lifetime and persisted rollback checks immediately before preparation.
            var descriptor = _verifier.VerifyDescriptor(discovery!.Raw, discovery.Signature);
            var files = descriptor.Files.Select(file => new UpdateFile(file.Path, checked((long)file.ByteLength), file.Sha256)).ToArray();
            if (files.Length is < 1 or > 128 || files.Any(file => !UpdateInstaller.IsRuntimePath(file.Path)))
                throw new InvalidDataException("Invalid managed runtime inventory.");
            UpdateInstaller.ReclaimAbandonedStages(installRoot);
            UpdateInstaller.PruneCompleted(installRoot);
            transactionId = Guid.NewGuid().ToString("N");
            var stage = UpdateInstaller.CreateStage(installRoot, transactionId);
            var transaction = Path.GetDirectoryName(stage)!;
            var descriptorPath = Path.Combine(transaction, "release.json");
            var signaturePath = Path.Combine(transaction, "release.json.sig");
            await WriteNewAsync(descriptorPath, discovery.Raw, cancellation).ConfigureAwait(false);
            await WriteNewAsync(signaturePath, discovery.Signature, cancellation).ConfigureAwait(false);
            var plan = UpdateInstaller.CreatePlan(installRoot, transactionId, files, restartArgs) with
            { DescriptorPath = descriptorPath, DescriptorSignaturePath = signaturePath };
            plan = _verifier.WithPreviousInventory(plan);
            UpdateInstaller.WritePlan(plan);
            var packagePath = Path.Combine(transaction, "package.zip");
            using (var package = new FileStream(packagePath, FileMode.CreateNew, FileAccess.ReadWrite, FileShare.None,
                65_536, FileOptions.Asynchronous))
            {
                progress.Report(new(UpdatePhase.Downloading, 0, discovery.Package.Size));
                await _source.DownloadAssetAsync(discovery.Package, package, GitHubReleases.MaxAssetBytes,
                    new DownloadProgress(progress, discovery.Package.Size), cancellation).ConfigureAwait(false);
                package.Position = 0;
                await ExtractAsync(package, plan, progress, cancellation).ConfigureAwait(false);
            }
            File.Delete(packagePath);
            if (!Usable(discovery)) throw new InvalidDataException("The release expired during staging.");
            await _verifier.VerifyAsync(plan, cancellation).ConfigureAwait(false);
            if (!Usable(discovery)) throw new InvalidDataException("The release expired during verification.");
            return plan;
        }
        catch
        {
            if (transactionId is not null) UpdateInstaller.DiscardPreparedStage(installRoot, transactionId);
            throw;
        }
        finally { _operation.Release(); }
    }

    private bool Usable(Discovery? discovery) => discovery is not null && DateTimeOffset.TryParseExact(discovery.Descriptor.ExpiresAt,
        "yyyy-MM-dd'T'HH:mm:ss.ffffff'Z'", CultureInfo.InvariantCulture, DateTimeStyles.AssumeUniversal | DateTimeStyles.AdjustToUniversal,
        out var expires) && _clock.GetUtcNow() < expires && discovery.AcceptedAt + _clock.GetElapsedTime(discovery.Timestamp) < expires;

    private static GitHubReleaseAsset Asset(GitHubRelease release, string name)
    {
        var assets = release.Assets.Where(asset => asset.Name == name).ToArray();
        return assets.Length == 1 ? assets[0] : throw new InvalidDataException("A required release asset is missing or duplicated.");
    }

    private static async Task WriteNewAsync(string path, byte[] bytes, CancellationToken cancellation)
    {
        UpdateInstaller.RequireNoReparse(path);
        using var stream = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.None, 4096, FileOptions.Asynchronous);
        await stream.WriteAsync(bytes, cancellation).ConfigureAwait(false);
    }

    private static async Task ExtractAsync(FileStream package, UpdateApplyPlan plan, IProgress<UpdateProgress> progress,
        CancellationToken cancellation)
    {
        CheckZipDirectory(package);
        using var archive = new ZipArchive(package, ZipArchiveMode.Read, leaveOpen: true);
        if (archive.Entries.Count > 128) throw new InvalidDataException("Too many archive entries.");
        var expected = plan.Files.ToDictionary(file => file.Path, StringComparer.Ordinal);
        var directories = expected.Keys.SelectMany(path => Parents(path)).ToHashSet(StringComparer.Ordinal);
        var seen = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        long total = 0;
        foreach (var file in plan.Files) total = checked(total + file.Size);
        if (total > MaxUnpackedBytes) throw new InvalidDataException("Unpacked runtime is too large.");
        long completed = 0;
        progress.Report(new(UpdatePhase.Verifying, completed, total));
        var buffer = new byte[65_536];
        foreach (var entry in archive.Entries)
        {
            cancellation.ThrowIfCancellationRequested();
            var path = entry.FullName;
            var directory = path.EndsWith('/');
            var unixType = (entry.ExternalAttributes >> 16) & 0xf000;
            if ((entry.ExternalAttributes & (int)FileAttributes.ReparsePoint) != 0
                || unixType != 0 && unixType != (directory ? 0x4000 : 0x8000))
                throw new InvalidDataException("Archive links and special files are forbidden.");
            if (!seen.Add(path.TrimEnd('/'))) throw new InvalidDataException("Duplicate archive path.");
            if (directory)
            {
                if (!directories.Contains(path[..^1]) || entry.Length != 0) throw new InvalidDataException("Unexpected archive directory.");
                continue;
            }
            if (!expected.TryGetValue(path, out var file) || !UpdateInstaller.IsRuntimePath(path) || entry.Length != file.Size)
                throw new InvalidDataException("Unexpected or incorrectly sized archive file.");
            var outputPath = Path.Combine(plan.StageRoot, path.Replace('/', Path.DirectorySeparatorChar));
            UpdateInstaller.RequireNoReparse(outputPath);
            Directory.CreateDirectory(Path.GetDirectoryName(outputPath)!);
            UpdateInstaller.RequireNoReparse(outputPath);
            using var input = entry.Open();
            using var output = new FileStream(outputPath, FileMode.CreateNew, FileAccess.Write, FileShare.None, buffer.Length, FileOptions.Asynchronous);
            using var hash = IncrementalHash.CreateHash(HashAlgorithmName.SHA256);
            long written = 0;
            for (;;)
            {
                var read = await input.ReadAsync(buffer, cancellation).ConfigureAwait(false);
                if (read == 0) break;
                written = checked(written + read);
                if (written > file.Size) throw new InvalidDataException("Archive output exceeds its signed size.");
                hash.AppendData(buffer, 0, read);
                await output.WriteAsync(buffer.AsMemory(0, read), cancellation).ConfigureAwait(false);
                completed += read;
                progress.Report(new(UpdatePhase.Verifying, completed, total));
            }
            if (written != file.Size || Convert.ToHexStringLower(hash.GetHashAndReset()) != file.Sha256)
                throw new InvalidDataException("Archive output does not match its signed digest.");
            expected.Remove(path);
        }
        if (expected.Count != 0) throw new InvalidDataException("Archive inventory is incomplete.");
    }

    // Bound central-directory parsing before ZipArchive allocates an object for every entry. ZIP64 is unnecessary within our limits.
    private static void CheckZipDirectory(FileStream package)
    {
        if (package.Length < 22) throw new InvalidDataException("Invalid archive.");
        var tail = new byte[(int)Math.Min(package.Length, 65_557)];
        package.Position = package.Length - tail.Length;
        package.ReadExactly(tail);
        for (var offset = tail.Length - 22; offset >= 0; offset--)
        {
            var end = tail.AsSpan(offset);
            if (BinaryPrimitives.ReadUInt32LittleEndian(end) != 0x06054b50 || offset + 22 + BinaryPrimitives.ReadUInt16LittleEndian(end[20..]) != tail.Length) continue;
            var entries = BinaryPrimitives.ReadUInt16LittleEndian(end[10..]);
            var centralSize = BinaryPrimitives.ReadUInt32LittleEndian(end[12..]);
            var centralOffset = BinaryPrimitives.ReadUInt32LittleEndian(end[16..]);
            if (BinaryPrimitives.ReadUInt16LittleEndian(end[4..]) != 0 || BinaryPrimitives.ReadUInt16LittleEndian(end[6..]) != 0
                || BinaryPrimitives.ReadUInt16LittleEndian(end[8..]) != entries || entries is < 1 or > 128
                || centralSize > 1_048_576 || (long)centralOffset + centralSize != package.Length - tail.Length + offset)
                throw new InvalidDataException("Invalid or oversized archive directory.");
            package.Position = 0;
            return;
        }
        throw new InvalidDataException("Invalid archive directory.");
    }

    private static IEnumerable<string> Parents(string path)
    {
        for (var separator = path.IndexOf('/'); separator >= 0; separator = path.IndexOf('/', separator + 1))
            yield return path[..separator];
    }

    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        _source.Dispose();
        _operation.Dispose();
    }
    private sealed record Discovery(LauncherReleaseDescriptor Descriptor, byte[] Raw, byte[] Signature,
        GitHubReleaseAsset Package, long Timestamp, DateTimeOffset AcceptedAt);
    private sealed class DownloadProgress(IProgress<UpdateProgress> progress, long total) : IProgress<long>
    {
        public void Report(long value) => progress.Report(new(UpdatePhase.Downloading, value, total));
    }
}
