// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.Json;

namespace Infinity.Core.Diagnostics;

/// <summary>Local-only model of one abnormal game exit (never sent anywhere; OD-241).</summary>
public sealed record CrashIncident(string Id, string OwnerAccountId, string SessionId, string Game, string Xuid,
    DateTimeOffset OccurredAt, string LauncherVersion, string? HookVersion, string? AzimuthVersion, uint ExitCode,
    CrashDumpInfo? Dump, string DumpAbsentReason, CrashContextSnapshot Context);

/// <summary>
/// The launcher's local crash reports (<c>&lt;state&gt;\crash-reports\&lt;id&gt;</c>) and the list of crashes the player already handled
/// (<c>&lt;state&gt;\crash-handled.txt</c>, OD-241 "Ignore" or "Get logs"). A handled crash never shows again, and a later capture of the
/// same minidump is never stored again (<see cref="HandledKey(CrashDumpInfo)"/> is derived from the dump itself).
/// </summary>
public sealed class CrashIncidentStore(string stateDirectory)
{
    public const int MaxMetadataBytes = 64 * 1024;
    public const int MaxIncidents = 8;
    public const int MaxHandledKeys = 256;
    public const string HandledFileName = "crash-handled.txt";
    private static readonly JsonSerializerOptions Json = new(JsonSerializerDefaults.Web);
    private readonly string _state = Path.GetFullPath(stateDirectory);
    private string Root => Path.Combine(_state, "crash-reports");
    private string HandledPath => Path.Combine(_state, HandledFileName);

    private string Folder(string id) => Guid.TryParseExact(id, "D", out _) ? Path.Combine(Root, id) : throw new InvalidDataException("Invalid incident id");

    /// <summary>
    /// The stable id of a crash: its minidump's own identity when one was matched (process id, dump time, exception, fault module and
    /// offset; the same .dmp always gives the same key), else the local incident id.
    /// </summary>
    public static string HandledKey(CrashIncident incident)
    {
        ArgumentNullException.ThrowIfNull(incident);
        return incident.Dump is { } dump ? HandledKey(dump) : "inc-" + incident.Id;
    }

    public static string HandledKey(CrashDumpInfo dump)
    {
        ArgumentNullException.ThrowIfNull(dump);
        var identity = string.Join('|', dump.ProcessId.ToString(CultureInfo.InvariantCulture),
            dump.TimestampUtc.ToUnixTimeSeconds().ToString(CultureInfo.InvariantCulture),
            dump.ExceptionCode?.ToString("x8", CultureInfo.InvariantCulture) ?? "-", dump.FaultModule?.ToLowerInvariant() ?? "-",
            dump.FaultModuleRva?.ToString("x", CultureInfo.InvariantCulture) ?? "-");
        return "dmp-" + Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(identity)))[..32];
    }

    /// <summary>
    /// Stores a new incident; false (nothing written) when its dump was already handled: a crash the player ignored is never rebuilt from
    /// the same .dmp.
    /// </summary>
    public bool Save(CrashIncident incident, ReadOnlySpan<byte> dump)
    {
        ArgumentNullException.ThrowIfNull(incident);
        if (dump.Length > CrashDumpCollector.MaxDumpBytes) throw new InvalidDataException("Dump size limit");
        if (IsHandled(HandledKey(incident))) return false;
        var bytes = JsonSerializer.SerializeToUtf8Bytes(incident, Json);
        if (bytes.Length > MaxMetadataBytes) throw new InvalidDataException("Metadata size limit");
        Directory.CreateDirectory(Root);
        CheckDirectory(Root);
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
        return true;
    }

    /// <summary>The account's crashes not handled yet, newest first: what the crash screen shows.</summary>
    public IReadOnlyList<CrashIncident> Pending(string accountId)
    {
        var handled = Handled();
        return All().Where(i => i.OwnerAccountId == accountId && !handled.Contains(HandledKey(i))).ToArray();
    }

    /// <summary>Every valid local incident on this PC, newest first (handled ones included): what "Get logs" projects into the zip.</summary>
    public IReadOnlyList<CrashIncident> All()
    {
        if (!Directory.Exists(Root)) return [];
        CheckDirectory(Root);
        Prune();
        var result = new List<CrashIncident>();
        foreach (var folder in Directory.EnumerateDirectories(Root).Take(64))
        {
            try
            {
                CheckDirectory(folder);
                var path = Path.Combine(folder, "metadata.json");
                var info = new FileInfo(path);
                if (!info.Exists || info.Length > MaxMetadataBytes || (info.Attributes & FileAttributes.ReparsePoint) != 0) continue;
                var incident = JsonSerializer.Deserialize<CrashIncident>(File.ReadAllBytes(path), Json);
                if (incident is null || incident.OwnerAccountId is null || Folder(incident.Id) != folder
                    || incident.Context is null || incident.Context.Observations is null
                    || incident.Context.Observations.Count > CrashContext.MaxObservations
                    || incident.Context.World is { } world && (world.Participants is null || world.Participants.Count > 12
                        || world.Participants.Any(p => p is null || p.Gamertag is null || p.Xuid is null || p.Role is null))) continue;
                result.Add(incident);
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException or ArgumentException) { /* An incomplete local incident is skipped. */ }
        }
        return result.OrderByDescending(i => i.OccurredAt).Take(MaxIncidents).ToArray();
    }

    /// <summary>The incident's stored dump, only when it is still the exact minidump the incident describes.</summary>
    public byte[]? ReadDump(CrashIncident incident)
    {
        var folder = Folder(incident.Id); CheckDirectory(Root); CheckDirectory(folder);
        var path = Path.Combine(folder, "crash.dmp"); var info = new FileInfo(path);
        if (!info.Exists || info.Length > CrashDumpCollector.MaxDumpBytes || (info.Attributes & FileAttributes.ReparsePoint) != 0) return null;
        using var file = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
        if (file.Length > CrashDumpCollector.MaxDumpBytes) return null;
        var data = new byte[checked((int)file.Length)]; file.ReadExactly(data);
        return MinidumpInspector.TryRead(data, out var parsed, out _) && parsed == incident.Dump ? data : null;
    }

    public bool IsHandled(string key) => Handled().Contains(key);

    /// <summary>OD-241: the crash never shows again (persisted before the screen closes; the newest <see cref="MaxHandledKeys"/> are kept).</summary>
    public void MarkHandled(CrashIncident incident)
    {
        var key = HandledKey(incident);
        var keys = Handled().ToList();
        if (keys.Contains(key)) return;
        keys.Add(key);
        Directory.CreateDirectory(_state);
        var temporary = HandledPath + ".tmp";
        File.WriteAllText(temporary, string.Join('\n', keys.TakeLast(MaxHandledKeys)) + "\n", new UTF8Encoding(false));
        File.Move(temporary, HandledPath, overwrite: true);
    }

    private HashSet<string> Handled()
    {
        var info = new FileInfo(HandledPath);
        if (!info.Exists) return [];
        if (info.Length > 64 * 1024 || (info.Attributes & FileAttributes.ReparsePoint) != 0) throw new InvalidDataException("Invalid handled crash list");
        return File.ReadAllLines(HandledPath).Select(l => l.Trim()).Where(ValidKey).ToHashSet(StringComparer.Ordinal);
    }

    private static bool ValidKey(string key) => key.Length is > 4 and <= 64 && (key.StartsWith("dmp-", StringComparison.Ordinal) || key.StartsWith("inc-", StringComparison.Ordinal))
        && key.All(c => char.IsAsciiLetterOrDigit(c) || c == '-');

    /// <summary>Removes the incident's folder: every plain file in it (older launchers' files too), then the folder once empty.</summary>
    public void Delete(string id)
    {
        var folder = Folder(id);
        if (!Directory.Exists(folder)) return;
        CheckDirectory(Root); CheckDirectory(folder);
        foreach (var file in new DirectoryInfo(folder).EnumerateFiles().Take(64))
            if ((file.Attributes & FileAttributes.ReparsePoint) == 0) file.Delete();
        if (!Directory.EnumerateFileSystemEntries(folder).Any()) Directory.Delete(folder);
    }

    private void Prune()
    {
        foreach (var item in Directory.EnumerateDirectories(Root).Take(64).Select(p => new DirectoryInfo(p)).OrderByDescending(d => d.CreationTimeUtc).Select((d, i) => (d, i)).ToArray())
            if (Guid.TryParseExact(item.d.Name, "D", out _) && (item.i >= MaxIncidents || item.d.CreationTimeUtc < DateTime.UtcNow.AddDays(-7))) Delete(item.d.Name);
    }

    private static void CheckDirectory(string path)
    {
        if ((File.GetAttributes(path) & FileAttributes.ReparsePoint) != 0) throw new IOException("Diagnostic directory must not be a link");
    }
}
