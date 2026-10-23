// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Text;
using System.Text.Json;
using Infinity.Core.Process;

namespace Infinity.Core.Mods;

public sealed record SessionDatabaseJournal(string AzimuthRoot, string GameRoot, string Target, string Baseline, string Prepared, bool Existed,
    IReadOnlyList<string>? CreatedDirectories = null);

public sealed class NativeModPlan : IDisposable
{
    public static IReadOnlyList<string> EnvironmentNames { get; } = ["FI_MOD_PLAN", "FI_MOD_PLAN_HASH", "FI_MOD_RUNTIME", "FI_MOD_RUNTIME_HASH"];
    private readonly List<IDisposable> _locks;
    private readonly Action _restore;
    private bool _disposed;
    public IReadOnlyList<(string Name, string Value)> Environment { get; }
    internal NativeModPlan(string path, byte[] bytes, string runtime, List<IDisposable> locks, Action restore)
    {
        _locks = locks; _restore = restore;
        using (var file = new FileStream(path, FileMode.CreateNew, FileAccess.Write, FileShare.None)) { file.Write(bytes); file.Flush(true); }
        locks.Add(new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read));
        Environment = [("FI_MOD_PLAN", path), ("FI_MOD_PLAN_HASH", ModManifest.Hash(bytes)), ("FI_MOD_RUNTIME", runtime), ("FI_MOD_RUNTIME_HASH", ModManifest.Hash(File.ReadAllBytes(runtime)))];
    }
    public void Dispose()
    {
        if (_disposed) return; _disposed = true;
        try { _restore(); } finally { foreach (var item in _locks.AsEnumerable().Reverse()) item.Dispose(); }
    }
    internal static void Text(BinaryWriter writer, string text)
    {
        var bytes = Encoding.Unicode.GetBytes(text);
        if (bytes.Length is < 2 or > 65534 || text.Contains('\0', StringComparison.Ordinal)) throw new ModException("invalid launch text");
        writer.Write((uint)(bytes.Length / 2)); writer.Write(bytes);
    }
}

public sealed partial class ManagedModFiles
{
    public bool HasEnabledNativeMods => InstalledManifests().Any(m => m.IsNative && Receipt(m) is { Enabled: true });
    private string SessionJournal => Path.Combine(BackupRoot, "database-session.json");
    private static void NoDatabaseSidecars(string path)
    {
        foreach (var suffix in new[] { "-wal", "-shm", "-journal" })
            if (File.Exists(path + suffix)) throw new ModException("close SQLite writers and checkpoint the database first");
    }
    private byte[] DatabaseBytes(string path)
    {
        RejectLinks(path); NoDatabaseSidecars(path);
        var size = new FileInfo(path).Length;
        if (size is < 100 or > 64 * 1024 * 1024) throw new ModException("invalid database size");
        var bytes = File.ReadAllBytes(path);
        if (!bytes.AsSpan(0, 16).SequenceEqual("SQLite format 3\0"u8) || bytes[18] != 1 || bytes[19] != 1)
            throw new ModException("database requires a standalone checkpointed SQLite file");
        return bytes;
    }
    private SessionDatabaseJournal ReadSessionJournal()
    {
        RejectLinks(SessionJournal);
        var journal = JsonSerializer.Deserialize<SessionDatabaseJournal>(File.ReadAllBytes(SessionJournal)) ?? throw new ModException("invalid database session journal", ModErrors.Recovery);
        if (!string.Equals(journal.AzimuthRoot, _azimuth, StringComparison.OrdinalIgnoreCase)
            || !string.Equals(journal.GameRoot, _game, StringComparison.OrdinalIgnoreCase)) throw new ModException("database session belongs to another installation", ModErrors.Recovery);
        return journal;
    }

    /// <summary>
    /// Ends a session database: the baseline returns when an override existed before; otherwise the
    /// session's own copy is removed with the folders the session created (M11), so nothing shadows
    /// the stock database afterwards. The stock source is never touched.
    /// </summary>
    private void RestoreSessionDatabase()
    {
        CheckGame();
        if (!File.Exists(SessionJournal)) return;
        var journal = ReadSessionJournal();
        var target = Safe(_game!, journal.Target); NoDatabaseSidecars(target);
        var current = Current(target);
        if (current != journal.Baseline && current != journal.Prepared && !(current is null && !journal.Existed))
            throw new ModException("session database changed outside the runtime; preserved for recovery", ModErrors.Recovery);
        var original = ObjectBytes(journal.Baseline); _ = ObjectBytes(journal.Prepared);
        CheckGame();
        FinishSessionDatabase(journal, target, original);
    }

    private void FinishSessionDatabase(SessionDatabaseJournal journal, string target, byte[] original)
    {
        if (journal.Existed) Atomic(target, original);
        else
        {
            RemoveFile(target);
            foreach (var directory in (journal.CreatedDirectories ?? []).OrderByDescending(d => d.Length))
            {
                var path = Safe(_game!, directory);
                if (Directory.Exists(path) && !Directory.EnumerateFileSystemEntries(path).Any()) Directory.Delete(path, recursive: false);
            }
        }
        File.Move(SessionJournal, Path.Combine(BackupRoot, "database-session-" + Guid.NewGuid().ToString("N") + ".json"));
    }

    /// <summary>
    /// H4c resolution for a refused restore: the current database and its SQLite sidecars are kept as
    /// backup objects (listed in the archived record), then the original state returns as above.
    /// </summary>
    private void ResolveSessionDatabase()
    {
        CheckGame();
        if (!File.Exists(SessionJournal)) return;
        SessionDatabaseJournal journal;
        try { journal = ReadSessionJournal(); }
        catch (Exception error) when (error is ModException or JsonException)
        {
            File.Move(SessionJournal, Path.Combine(BackupRoot, "database-session-unreadable-" + Guid.NewGuid().ToString("N") + ".json"));
            return;
        }
        var target = Safe(_game!, journal.Target);
        var kept = new List<string>();
        foreach (var path in new[] { target, target + "-wal", target + "-shm", target + "-journal" })
        {
            if (!File.Exists(path)) continue;
            if (new FileInfo(path).Length > 512L * 1024 * 1024) throw new ModException("database too large to keep a backup", ModErrors.Recovery);
            kept.Add(Path.GetFileName(path) + "=" + Store(File.ReadAllBytes(path)));
        }
        Atomic(Path.Combine(BackupRoot, "database-session-kept-" + Guid.NewGuid().ToString("N") + ".json"), JsonSerializer.SerializeToUtf8Bytes(kept));
        foreach (var sidecar in new[] { "-wal", "-shm", "-journal" }) RemoveFile(target + sidecar);
        FinishSessionDatabase(journal, target, ObjectBytes(journal.Baseline));
    }
    public NativeModPlan PrepareNativeLaunch(string runtime, string helper, byte[] verificationKey)
    {
        runtime = Path.GetFullPath(runtime); helper = Path.GetFullPath(helper);
        if (!string.Equals(runtime, Path.Combine(Path.GetDirectoryName(helper)!, "infinity_mod.dll"), StringComparison.OrdinalIgnoreCase)
            || !Path.GetFileName(helper).Equals("infinity-mod-prepare.exe", StringComparison.OrdinalIgnoreCase))
            throw new ModException("preparation helper and verified runtime must be adjacent");
        var locks = new List<IDisposable> { Lock() };
        try
        {
            foreach (var path in new[] { runtime, helper })
            {
                RejectLinks(path, Path.GetDirectoryName(helper)!, linkedBoundary: false); locks.Add(new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read));
                if (!ReleaseSignature.VerifyFile(path, path + ".sig", verificationKey)) throw new ModException("mod host signature does not verify");
            }
            var enabled = InstalledManifests().Where(m => Receipt(m) is { Enabled: true }).ToArray();
            var remaining = enabled.Where(m => m.IsNative).OrderBy(m => m.Id, StringComparer.Ordinal).ToList();
            var selected = new List<ModManifest>();
            while (remaining.Count > 0)
            {
                var next = remaining.FirstOrDefault(m => !m.Dependencies.Any(d => remaining.Any(n => n.Id == d.Id)));
                if (next is null) throw new ModException("cyclic native dependencies");
                selected.Add(next); remaining.Remove(next);
            }
            if (selected.Count is < 1 or > 32) throw new ModException("invalid selected native mod count");
            foreach (var mod in enabled)
            {
                if (!Verified(mod)) throw new ModException("mod installation changed");
                CheckDependencies(mod);
                foreach (var file in mod.Files.Where(f => f.Role == "binary"))
                {
                    var path = Target(file.Root, file.Destination); locks.Add(new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read));
                    if (Current(path) != file.Hash) throw new ModException("binary changed before launch");
                }
                if (mod.ConfigPath is { } config) locks.Add(new FileStream(Target("azimuth", config), FileMode.Open, FileAccess.Read, FileShare.Read));
            }
            var policies = selected.Where(m => m.Document.TryGetProperty("database", out _)).Select(m => m.Document.GetProperty("database")).ToArray();
            string? relative = null, targetDatabase = null, baseline = null, before = null;
            var createdDirectories = new List<string>();
            var hashes = new byte[64];
            var folder = Safe(BackupRoot, "sessions/" + Guid.NewGuid().ToString("N")); Directory.CreateDirectory(folder);
            if (policies.Length > 0)
            {
                relative = policies[0].GetProperty("path").GetString()!;
                var seed = policies[0].GetProperty("seedFrom").GetString()!;
                if (policies.Any(p => p.GetProperty("root").GetString() != "game" || p.GetProperty("access").GetString() != "startup"
                    || p.GetProperty("path").GetString() != relative || p.GetProperty("seedFrom").GetString() != seed))
                    throw new ModException("this runtime supports one shared startup database per session");
                targetDatabase = Safe(_game ?? throw new ModException("game folder missing"), relative);
                before = Current(targetDatabase);
                var source = before is null ? Safe(_game!, seed) : targetDatabase;
                for (var parent = Path.GetDirectoryName(relative.Replace('/', Path.DirectorySeparatorChar)); !string.IsNullOrEmpty(parent); parent = Path.GetDirectoryName(parent))
                    if (!Directory.Exists(Path.Combine(_game!, parent))) createdDirectories.Add(parent.Replace(Path.DirectorySeparatorChar, '/'));
                var bytes = DatabaseBytes(source); baseline = Store(bytes);
                if (before is not null && before != baseline) throw new ModException("database changed during snapshot");
                Atomic(Path.Combine(folder, "baseline.sqlite"), bytes);
            }
            byte[] Plan()
            {
                using var bytes = new MemoryStream(); using var writer = new BinaryWriter(bytes, Encoding.Unicode, leaveOpen: true);
                writer.Write("FIMOD002"u8); writer.Write((uint)selected.Count);
                NativeModPlan.Text(writer, _azimuth); NativeModPlan.Text(writer, _game ?? _azimuth);
                writer.Write(targetDatabase is null ? 0u : 1u); if (targetDatabase is not null) NativeModPlan.Text(writer, targetDatabase);
                writer.Write(baseline is null ? new byte[32] : Convert.FromHexString(baseline)); writer.Write(hashes);
                var readiness = enabled.Where(m => m.Document.TryGetProperty("readiness", out _)).Select(m => m.Document.GetProperty("readiness")).ToArray();
                writer.Write((uint)readiness.Length);
                foreach (var r in readiness)
                {
                    NativeModPlan.Text(writer, Target("azimuth", r.GetProperty("module").GetString()!));
                    NativeModPlan.Text(writer, Target("azimuth", r.GetProperty("log").GetString()!));
                    NativeModPlan.Text(writer, r.GetProperty("contains").GetString()!);
                }
                for (var i = 0; i < selected.Count; i++)
                {
                    var mod = selected[i]; var binary = Target("azimuth", mod.Document.GetProperty("nativeHost").GetProperty("binary").GetString()!);
                    var settings = ReadSettings(mod);
                    NativeModPlan.Text(writer, mod.Id); NativeModPlan.Text(writer, mod.Version); NativeModPlan.Text(writer, binary); NativeModPlan.Text(writer, Target("azimuth", mod.ConfigPath!));
                    writer.Write(Convert.FromHexString(Current(binary)!)); writer.Write(Convert.FromHexString(settings.SourceHash));
                    writer.Write(Convert.FromHexString(mod.Multiplayer!.Fingerprint(mod, settings.Values)));
                    uint mask = 0; for (var j = 0; j < i; j++) if (mod.Dependencies.Any(d => d.Id == selected[j].Id)) mask |= 1u << j;
                    writer.Write(mask);
                    var tables = mod.Document.TryGetProperty("database", out var db) ? db.GetProperty("tables").EnumerateArray().Select(t => t.GetString()!).ToArray() : [];
                    writer.Write((uint)tables.Length); foreach (var table in tables) NativeModPlan.Text(writer, table);
                }
                writer.Flush(); return bytes.ToArray();
            }
            if (targetDatabase is not null)
            {
                var prepare = Path.Combine(folder, "prepare.bin"); var plan = Plan(); Atomic(prepare, plan);
                using var planLock = new FileStream(prepare, FileMode.Open, FileAccess.Read, FileShare.Read);
                var start = new ProcessStartInfo(helper) { UseShellExecute = false, CreateNoWindow = true, WindowStyle = ProcessWindowStyle.Hidden, WorkingDirectory = Path.GetDirectoryName(helper)! };
                start.ArgumentList.Add(prepare); start.ArgumentList.Add(ModManifest.Hash(plan));
                using var process = System.Diagnostics.Process.Start(start) ?? throw new ModException("database preparation did not start");
                if (!process.WaitForExit(60000)) { process.Kill(entireProcessTree: true); process.WaitForExit(); throw new ModException("database preparation timed out"); }
                if (process.ExitCode != 0) throw new ModException("native database preparation failed");
                hashes = File.ReadAllBytes(Safe(folder, "database.hashes"));
                var prepared = DatabaseBytes(Safe(folder, "session.sqlite")); var after = Store(prepared);
                if (hashes.Length != 64 || Convert.ToHexStringLower(hashes.AsSpan(0, 32)) != after || hashes.AsSpan(32).IndexOfAnyExcept((byte)0) < 0)
                    throw new ModException("prepared database result differs");
                CheckGame(); NoDatabaseSidecars(targetDatabase);
                if (Current(targetDatabase) != before) throw new ModException("database changed before publication");
                Atomic(SessionJournal, JsonSerializer.SerializeToUtf8Bytes(new SessionDatabaseJournal(_azimuth, _game!, relative!, baseline!, after, before is not null, createdDirectories), Json));
                Atomic(targetDatabase, prepared);
            }
            return new(Path.Combine(folder, "launch.bin"), Plan(), runtime, locks, RestoreSessionDatabase);
        }
        catch
        {
            try { if (File.Exists(SessionJournal)) RestoreSessionDatabase(); }
            finally { foreach (var item in locks.AsEnumerable().Reverse()) item.Dispose(); }
            throw;
        }
    }
}
