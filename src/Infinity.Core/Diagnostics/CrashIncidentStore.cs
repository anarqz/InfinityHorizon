// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Diagnostics;

/// <summary>Local-only model. Network DTOs are defined separately by the platform contract.</summary>
public sealed record CrashIncident(string Id, string OwnerAccountId, string SessionId, string Game, string Xuid,
    DateTimeOffset OccurredAt, string LauncherVersion, string? HookVersion, string? AzimuthVersion, uint ExitCode,
    CrashDumpInfo? Dump, string DumpAbsentReason, CrashContextSnapshot Context);

/// <summary>No upload on capture or load. Files are private local diagnostics until an explicit Send action.</summary>
public sealed class CrashIncidentStore(string stateDirectory)
{
    public const int MaxMetadataBytes = 64 * 1024;
    public const int MaxIncidents = 8;
    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);
    private readonly string _root = Path.Combine(Path.GetFullPath(stateDirectory), "crash-reports");

    private string Folder(string id) => Guid.TryParseExact(id, "D", out _) ? Path.Combine(_root, id) : throw new InvalidDataException("Invalid incident id");

    public void Save(CrashIncident incident, ReadOnlySpan<byte> dump)
    {
        ArgumentNullException.ThrowIfNull(incident);
        if (dump.Length > CrashDumpCollector.MaxDumpBytes) throw new InvalidDataException("Dump size limit");
        var bytes = JsonSerializer.SerializeToUtf8Bytes(incident, Json);
        if (bytes.Length > MaxMetadataBytes) throw new InvalidDataException("Metadata size limit");
        Directory.CreateDirectory(_root);
        CheckDirectory(_root);
        var folder = Folder(incident.Id);
        Directory.CreateDirectory(folder);
        CheckDirectory(folder);
        var createdMetadata = false;
        try
        {
            using (var file = new FileStream(Path.Combine(folder, "metadata.json"), FileMode.CreateNew, FileAccess.Write, FileShare.None))
            { createdMetadata = true; file.Write(bytes); }
            if (dump.Length != 0)
                using (var file = new FileStream(Path.Combine(folder, "crash.dmp"), FileMode.CreateNew, FileAccess.Write, FileShare.None)) file.Write(dump);
        }
        catch { if (createdMetadata) Delete(incident.Id); throw; }
        Prune();
    }

    public IReadOnlyList<CrashIncident> Pending(string accountId)
    {
        if (!Directory.Exists(_root)) return [];
        CheckDirectory(_root);
        Prune();
        var result = new List<CrashIncident>();
        foreach (var folder in Directory.EnumerateDirectories(_root).Take(64))
        {
            try
            {
                CheckDirectory(folder);
                var path = Path.Combine(folder, "metadata.json");
                var info = new FileInfo(path);
                if (!info.Exists || info.Length > MaxMetadataBytes || (info.Attributes & FileAttributes.ReparsePoint) != 0) continue;
                var incident = JsonSerializer.Deserialize<CrashIncident>(File.ReadAllBytes(path), Json);
                if (incident is null || incident.OwnerAccountId != accountId || Folder(incident.Id) != folder
                    || incident.Context is null || incident.Context.Observations is null
                    || incident.Context.Observations.Count > CrashContext.MaxObservations
                    || incident.Context.World is { } world && (world.Participants is null || world.Participants.Count > 12
                        || world.Participants.Any(p => p is null || p.Gamertag is null || p.Xuid is null || p.Role is null))) continue;
                result.Add(incident);
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException or ArgumentException) { /* An incomplete local incident is not submitted. */ }
        }
        return result.OrderByDescending(i => i.OccurredAt).Take(MaxIncidents).ToArray();
    }

    public byte[]? ReadDump(CrashIncident incident)
    {
        var folder = Folder(incident.Id); CheckDirectory(_root); CheckDirectory(folder);
        var path = Path.Combine(folder, "crash.dmp"); var info = new FileInfo(path);
        if (!info.Exists || info.Length > CrashDumpCollector.MaxDumpBytes || (info.Attributes & FileAttributes.ReparsePoint) != 0) return null;
        using var file = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
        if (file.Length > CrashDumpCollector.MaxDumpBytes) return null;
        var data = new byte[checked((int)file.Length)]; file.ReadExactly(data);
        return MinidumpInspector.TryRead(data, out var parsed, out _) && parsed == incident.Dump ? data : null;
    }

    public CrashSubmission? Prepared(string id)
    {
        var folder = Folder(id); CheckDirectory(_root); CheckDirectory(folder);
        var current = Path.Combine(folder, "metadata-submission.json");
        var path = File.Exists(current) ? current : Path.Combine(folder, "submission.json"); var info = new FileInfo(path);
        if (!info.Exists) return null;
        if (info.Length > MaxMetadataBytes || (info.Attributes & FileAttributes.ReparsePoint) != 0) throw new InvalidDataException("Invalid local submission");
        var draft = JsonSerializer.Deserialize<CrashSubmission>(File.ReadAllBytes(path), Json);
        if (!Guid.TryParseExact(draft?.Request?.ReportId, "D", out var reportId) || reportId == Guid.Empty)
            throw new InvalidDataException("Invalid local submission");
        return draft;
    }

    public void Prepare(CrashSubmission submission, string? incidentId = null)
    {
        ArgumentNullException.ThrowIfNull(submission);
        if (!submission.IsCurrentMetadata) throw new InvalidDataException("Metadata-only consent is required");
        var folder = Folder(incidentId ?? submission.Request.ReportId); CheckDirectory(_root); CheckDirectory(folder);
        var bytes = JsonSerializer.SerializeToUtf8Bytes(submission, Json);
        if (bytes.Length > MaxMetadataBytes) throw new InvalidDataException("Submission size limit");
        WriteNew(Path.Combine(folder, "metadata-submission.json"), bytes);
    }

    public string? SentReportId(string id)
    {
        var folder = Folder(id); CheckDirectory(_root); CheckDirectory(folder);
        var path = Path.Combine(folder, "accepted-report.txt"); var info = new FileInfo(path);
        if (!info.Exists) return null;
        if (info.Length != 36 || (info.Attributes & FileAttributes.ReparsePoint) != 0) throw new InvalidDataException("Invalid local receipt");
        var reportId = File.ReadAllText(path);
        return Guid.TryParseExact(reportId, "D", out var parsed) && parsed != Guid.Empty ? reportId : throw new InvalidDataException("Invalid local receipt");
    }

    public void MarkSent(string incidentId, string reportId)
    {
        if (!Guid.TryParseExact(reportId, "D", out var parsed) || parsed == Guid.Empty) throw new InvalidDataException("Invalid report id");
        var folder = Folder(incidentId); CheckDirectory(_root); CheckDirectory(folder);
        if (SentReportId(incidentId) == reportId) return;
        WriteNew(Path.Combine(folder, "accepted-report.txt"), System.Text.Encoding.ASCII.GetBytes(reportId));
    }

    public void Delete(string id)
    {
        var folder = Folder(id);
        if (!Directory.Exists(folder)) return;
        CheckDirectory(_root); CheckDirectory(folder);
        foreach (var name in new[] { "metadata.json", "crash.dmp", "submission.json", "metadata-submission.json", "accepted-report.txt" })
        {
            var path = Path.Combine(folder, name);
            if (File.Exists(path)) File.Delete(path);
        }
        if (!Directory.EnumerateFileSystemEntries(folder).Any()) Directory.Delete(folder);
    }

    private void Prune()
    {
        foreach (var item in Directory.EnumerateDirectories(_root).Take(64).Select(p => new DirectoryInfo(p)).OrderByDescending(d => d.CreationTimeUtc).Select((d, i) => (d, i)).ToArray())
            if (Guid.TryParseExact(item.d.Name, "D", out _) && (item.i >= MaxIncidents || item.d.CreationTimeUtc < DateTime.UtcNow.AddDays(-7))) Delete(item.d.Name);
    }

    private static void WriteNew(string path, byte[] bytes)
    {
        var created = false;
        try
        {
            using var file = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.None);
            created = true; file.Write(bytes); file.Flush(true);
        }
        catch { if (created) File.Delete(path); throw; }
    }

    private static void CheckDirectory(string path)
    {
        if ((File.GetAttributes(path) & FileAttributes.ReparsePoint) != 0) throw new IOException("Diagnostic directory must not be a link");
    }
}
