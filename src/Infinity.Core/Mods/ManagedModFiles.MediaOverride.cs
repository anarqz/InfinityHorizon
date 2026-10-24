// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>
/// One file under the game's media overrides that a Supporter + Hardcore session may not have (OD-115): a game-relative path
/// (or a mod id) and a stable reason: <c>unexpected</c> (no enabled client-only mod maps it), <c>changed</c> (mapped, other
/// bytes), <c>link</c> (a reparse point), <c>database</c> (an enabled mod's runtime session database), <c>state</c> (an
/// unreadable installation record) or <c>limit</c> (more entries than are checked).
/// </summary>
public sealed record MediaOverrideFinding(string Path, string Reason);

public sealed partial class ManagedModFiles
{
    /// <summary>The most entries (files and folders) below the media overrides the Hardcore check walks; more is a finding.</summary>
    public const int MaxMediaOverrideEntries = 20000;
    private const int MaxRememberedOverrideDigests = 4096;
    private sealed record OverrideStamp(string Path, long Length, long Modified, string Sha256);
    private string OverrideDigestsPath => System.IO.Path.Combine(BackupRoot, "mediaoverride-digests.json");

    /// <summary>
    /// OD-115, Supporter + Hardcore only (the caller applies <c>IntegrityPolicy</c>): every file under <c>&lt;game&gt;/mediaoverride</c>
    /// must be a mapped file of an enabled client-only managed mod (<see cref="ModMultiplayer.AffectsMultiplayer"/> false) with
    /// exactly its manifest's bytes. A hand-placed or changed override, a link, or an enabled mod's session database is a finding
    /// (ids and paths only). Read-only but for a digest cache next to the receipts: an unchanged file (size and modification
    /// time) is not rehashed. Not anti-cheat: the launcher refuses its own Hardcore launch (OD-111 compatibility evidence).
    /// </summary>
    public IReadOnlyList<MediaOverrideFinding> AuditMediaOverrides()
    {
        if (_game is null) return [];
        var findings = new List<MediaOverrideFinding>();
        var expected = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        if (Directory.Exists(BackupRoot))
            foreach (var path in Directory.EnumerateFiles(BackupRoot, "*.receipt.json").ToArray())
            {
                var id = System.IO.Path.GetFileName(path)[..^".receipt.json".Length];
                try
                {
                    if (ReadReceipt(id) is not { Enabled: true } receipt) continue;
                    var manifest = SnapshotFor(receipt);
                    if (ModMultiplayer.AffectsMultiplayer(manifest)) continue; // never enabled on Hardcore (refused before this check)
                    if (manifest.Document.TryGetProperty("database", out var database))
                        findings.Add(new(database.GetProperty("path").GetString()!, "database"));
                    foreach (var file in manifest.Files.Where(f => f.Root == "game" && ModManifest.UnderMediaOverride(f.Destination)))
                        expected[file.Destination] = file.Hash;
                }
                catch (Exception error) when (IsStateError(error)) { findings.Add(new(id, "state")); }
            }
        var root = System.IO.Path.Combine(_game, ModManifest.MediaOverrideFolder);
        if (!Directory.Exists(root) && !File.Exists(root)) return Sorted(findings);
        if (IsLink(root) || File.Exists(root)) { findings.Add(new(ModManifest.MediaOverrideFolder, "link")); return Sorted(findings); }
        var remembered = ReadOverrideDigests();
        var digests = new List<OverrideStamp>();
        var pending = new Stack<string>(); pending.Push(root);
        var entries = 0;
        while (pending.Count > 0)
        {
            var folder = pending.Pop();
            foreach (var entry in Directory.EnumerateFileSystemEntries(folder))
            {
                var relative = ModManifest.MediaOverrideFolder + "/" + System.IO.Path.GetRelativePath(root, entry).Replace('\\', '/');
                if (++entries > MaxMediaOverrideEntries) { findings.Add(new(ModManifest.MediaOverrideFolder, "limit")); pending.Clear(); break; }
                if (IsLink(entry)) { findings.Add(new(relative, "link")); continue; }
                if (Directory.Exists(entry)) { pending.Push(entry); continue; }
                if (!expected.TryGetValue(relative, out var hash)) { findings.Add(new(relative, "unexpected")); continue; }
                var info = new FileInfo(entry);
                var stamp = remembered.FirstOrDefault(r => string.Equals(r.Path, relative, StringComparison.OrdinalIgnoreCase)
                    && r.Length == info.Length && r.Modified == info.LastWriteTimeUtc.Ticks);
                string actual;
                var (length, modified) = (info.Length, info.LastWriteTimeUtc.Ticks);
                var stable = true;
                if (stamp is not null) actual = stamp.Sha256;
                else
                {
                    using (var input = new FileStream(entry, FileMode.Open, FileAccess.Read, FileShare.Read, 128 * 1024, FileOptions.SequentialScan))
                        actual = Convert.ToHexStringLower(SHA256.HashData(input));
                    info.Refresh();
                    // Changed while it was hashed: the digest is used for this verdict but never remembered with a newer stamp.
                    stable = info.Length == length && info.LastWriteTimeUtc.Ticks == modified;
                }
                if (stable) digests.Add(new(relative, length, modified, actual));
                if (!string.Equals(actual, hash, StringComparison.OrdinalIgnoreCase)) findings.Add(new(relative, "changed"));
            }
        }
        RememberOverrideDigests(digests);
        return Sorted(findings);
    }

    private static List<MediaOverrideFinding> Sorted(List<MediaOverrideFinding> findings) =>
        findings.OrderBy(f => f.Path, StringComparer.OrdinalIgnoreCase).ThenBy(f => f.Reason, StringComparer.Ordinal).ToList();

    private List<OverrideStamp> ReadOverrideDigests()
    {
        try
        {
            if (!File.Exists(OverrideDigestsPath) || IsLink(OverrideDigestsPath) || new FileInfo(OverrideDigestsPath).Length > 4 * 1024 * 1024) return [];
            return (JsonSerializer.Deserialize<List<OverrideStamp>>(File.ReadAllBytes(OverrideDigestsPath)) ?? [])
                .Where(s => s?.Path is not null && s.Sha256 is { Length: 64 }).ToList();
        }
        catch (Exception error) when (IsStateError(error)) { return []; }
    }

    /// <summary>A cache failure never changes the verdict: the next check rehashes.</summary>
    private void RememberOverrideDigests(List<OverrideStamp> digests)
    {
        try
        {
            if (!Directory.Exists(BackupRoot)) return;
            Atomic(OverrideDigestsPath, JsonSerializer.SerializeToUtf8Bytes(digests.Take(MaxRememberedOverrideDigests).ToList()));
        }
        catch (Exception error) when (IsStateError(error)) { }
    }
}
