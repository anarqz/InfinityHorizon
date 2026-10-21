// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;
using System.Text.Json.Serialization;

namespace Infinity.Core.Update;

/// <summary>Local installation input, never an HTTP contract or a replacement for signed release verification.</summary>
public sealed record UpdateFile(string Path, long Size, string Sha256);

/// <summary>The installer accepts only its own runtime paths and revalidates this plan through a trusted verifier.</summary>
public sealed record UpdateApplyPlan(string InstallRoot, string StageRoot, string TransactionId,
    IReadOnlyList<UpdateFile> Files, IReadOnlyList<string> RestartArguments)
{
    public string? DescriptorPath { get; init; }
    public string? DescriptorSignaturePath { get; init; }
    public IReadOnlyList<UpdateFile> PreviousFiles { get; init; } = [];
    public string? PreviousDescriptorPath { get; init; }
    public string? PreviousDescriptorSignaturePath { get; init; }
    public string? PreviousChecksumsPath { get; init; }
    public string? PreviousChecksumsSignaturePath { get; init; }
    public int ParentProcessId { get; init; }
    public long ParentStartTimeUtcTicks { get; init; }
    public string HealthNonce { get; init; } = string.Empty;
}

/// <summary>Must authenticate the exact release descriptor, version, inventory and binary signatures. No permissive default exists.</summary>
public interface IUpdatePlanVerifier
{
    Task VerifyAsync(UpdateApplyPlan plan, CancellationToken cancellation);
}

public enum UpdateInstallPhase { BackingUp, BackupReady, FileApplied, AwaitingStartup, Committed }

public sealed record UpdateInstallProgress(UpdateInstallPhase Phase, string? RelativePath = null);
public sealed record UpdateRecovery(string TransactionId, string PlanPath, string WorkerPath);

/// <summary>A failed update was rolled back, or needs an explicit recovery before Play.</summary>
public sealed class UpdateInstallException(string message, bool rollbackCompleted, Exception? inner = null)
    : IOException(message, inner)
{
    public bool RollbackCompleted { get; } = rollbackCompleted;
}

internal sealed record UpdateOriginal(string Path, bool Existed, long Size, string? Sha256, UpdateFile? Replacement);
internal sealed record UpdateJournal(string TransactionId, string Phase, IReadOnlyList<UpdateOriginal> Files);
internal sealed record UpdateHealth(string TransactionId, string Nonce, int ProcessId, long StartTimeUtcTicks);
internal sealed record UpdateParentReady(string TransactionId, string Nonce, int ProcessId, long StartTimeUtcTicks,
    int ParentProcessId, long ParentStartTimeUtcTicks);

internal static class UpdateJournalStore
{
    internal const int MaxDocumentBytes = 256 * 1024;
    internal static readonly JsonSerializerOptions Json = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.CamelCase,
        UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
        MaxDepth = 12,
    };

    internal static T Read<T>(string path)
    {
        UpdateInstaller.RequireNoReparse(path);
        using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
        if (stream.Length is <= 0 or > MaxDocumentBytes) throw new InvalidDataException("Invalid update document size.");
        return JsonSerializer.Deserialize<T>(stream, Json) ?? throw new InvalidDataException("Invalid update document.");
    }

    internal static void Write<T>(string path, T value)
    {
        var bytes = JsonSerializer.SerializeToUtf8Bytes(value, Json);
        if (bytes.Length > MaxDocumentBytes) throw new InvalidDataException("Update document is too large.");
        UpdateInstaller.RequireNoReparse(path);
        var temporary = path + ".tmp";
        UpdateInstaller.RequireNoReparse(temporary);
        if (File.Exists(temporary)) File.Delete(temporary); // unlink a leftover; never truncate a possible hard link
        using (var stream = new FileStream(temporary, FileMode.CreateNew, FileAccess.Write, FileShare.None,
                   4096, FileOptions.WriteThrough))
        {
            stream.Write(bytes);
            stream.Flush(flushToDisk: true);
        }
        UpdateInstaller.RequireNoReparse(path);
        File.Move(temporary, path, overwrite: true);
    }
}
