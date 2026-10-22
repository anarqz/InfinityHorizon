// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Xml;
using Infinity.Core.Process;

namespace Infinity.Core.PlayerData;

public enum DataCacheScope { Live, Offline, Discard }
public enum DataCacheFormat { Absent, CareerXml, Binary, Unreadable }
public sealed record DataCache(string RelativePath, DataCacheScope Scope, bool Career, long Bytes, DateTime? LastWriteUtc,
    DataCacheFormat Format, int? Entries);
public sealed record CacheInventory(IReadOnlyList<DataCache> Caches, int OtherFiles, long OtherBytes, bool Partial, bool RecoveryNeeded);

public sealed class CacheDiscovery(string azimuthRoot)
{
    private readonly DataFiles _files = new(azimuthRoot);

    public CacheInventory Scan(CancellationToken cancellation = default)
    {
        var caches = new List<DataCache>();
        foreach (var scope in Enum.GetValues<DataCacheScope>())
        foreach (var relative in ModeCaches.Fh2Caches)
        {
            cancellation.ThrowIfCancellationRequested();
            var location = scope switch
            {
                DataCacheScope.Offline => Path.Combine(ModeCaches.Fh2StashDir, "offline", relative),
                DataCacheScope.Discard => Path.Combine(ModeCaches.Fh2StashDir, "discard", relative),
                _ => relative,
            };
            var career = relative.EndsWith(".xml", StringComparison.Ordinal);
            try
            {
                var path = _files.Check(Path.Combine(_files.Root, location));
                var info = new FileInfo(path);
                if (!info.Exists) { caches.Add(new(location, scope, career, 0, null, DataCacheFormat.Absent, null)); continue; }
                var format = career ? DataCacheFormat.Unreadable : DataCacheFormat.Binary;
                int? entries = null;
                if (career && info.Length <= 4 * 1024 * 1024)
                {
                    using var input = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
                    using var reader = XmlReader.Create(input, new XmlReaderSettings
                    {
                        DtdProcessing = DtdProcessing.Prohibit, XmlResolver = null, MaxCharactersInDocument = 4 * 1024 * 1024,
                    });
                    reader.MoveToContent();
                    if (reader.LocalName == "DrivatarCache")
                    {
                        var count = 0;
                        while (reader.Read())
                        {
                            cancellation.ThrowIfCancellationRequested();
                            if (reader.NodeType == XmlNodeType.Element && reader.Depth == 1) count++;
                        }
                        entries = count;
                        format = DataCacheFormat.CareerXml;
                    }
                }
                caches.Add(new(location, scope, career, info.Length, info.LastWriteTimeUtc, format, entries));
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException or XmlException)
            { caches.Add(new(location, scope, career, 0, null, DataCacheFormat.Unreadable, null)); }
        }
        var otherFiles = 0;
        long otherBytes = 0;
        var partial = false;
        var scratch = Path.Combine(_files.Root, "data/titles/1A96545B/tmp/Turn10Temp.scratch");
        var pending = new Stack<string>();
        pending.Push(scratch);
        var visited = 0;
        while (pending.TryPop(out var directory))
        {
            cancellation.ThrowIfCancellationRequested();
            if (++visited > 10_000) { partial = true; break; }
            try
            {
                _files.Check(directory);
                if (!Directory.Exists(directory)) continue;
                foreach (var file in Directory.EnumerateFiles(directory))
                {
                    _files.Check(file);
                    if (ModeCaches.Fh2Caches.Any(relative => string.Equals(Path.GetFullPath(Path.Combine(_files.Root, relative)), Path.GetFullPath(file), StringComparison.OrdinalIgnoreCase))) continue;
                    if (++otherFiles > 10_000) { partial = true; break; }
                    otherBytes += new FileInfo(file).Length;
                }
                foreach (var child in Directory.EnumerateDirectories(directory)) pending.Push(child);
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException) { partial = true; }
            if (partial && otherFiles > 10_000) break;
        }
        var journal = _files.Check(Path.Combine(_files.Root, ModeCaches.Fh2StashDir, "journal.json"));
        var recoveryNeeded = File.Exists(journal) && (new FileInfo(journal).Length > 4096 || ModeCaches.Fh2(_files.Root).Online());
        return new(caches, otherFiles, otherBytes, partial, recoveryNeeded);
    }

    /// <summary>Moves only the two known cache kinds, in all three mode locations, to recoverable storage.</summary>
    public string Clear()
    {
        return Clear(new SaveMaintenance(_files.Root));
    }

    internal string Clear(SaveMaintenance controller)
    {
        using var maintenance = controller.Lock();
        if (Scan().RecoveryNeeded) throw new IOException("Finish the interrupted online cache switch before clearing caches.");
        var backup = _files.Check(Path.Combine(_files.Root, "data/fi-launcher/my-data-cache-backups", Guid.NewGuid().ToString("N")));
        var moved = new List<(string From, string To)>();
        try
        {
            foreach (var cache in Scan().Caches)
            {
                var source = _files.Check(Path.Combine(_files.Root, cache.RelativePath));
                if (!File.Exists(source)) continue;
                controller.EnsureIdle();
                var target = _files.Check(Path.Combine(backup, cache.RelativePath));
                _files.Move(source, target);
                moved.Add((source, target));
            }
        }
        catch
        {
            foreach (var (from, to) in moved.AsEnumerable().Reverse()) { controller.EnsureIdle(); _files.Move(to, from); }
            throw;
        }
        return backup;
    }
}
