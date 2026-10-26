using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>An uninstalled mod's files kept on this PC (owner, 2026-10-26): the manifest it was installed from and every file's bytes.</summary>
public sealed record HeldMod(string Id, string ManifestHash, IReadOnlyList<string> Objects);

public sealed partial class ManagedModFiles
{
    // Owner, 2026-10-26: "UNINSTALL remove the files from the game, not from the user PC. We should keep these files held somewhere
    // where they can be reused by the player. A 'purge' option should remove them from the user PC". The bytes are the backup
    // folder's content-addressed objects; a held-<id>.json record names them, so retention (Prune) never drops them.

    private string HeldPath(string id) => Safe(BackupRoot, "held-" + id + ".json");

    /// <summary>The held files of a mod not installed, when every one of them is still in the backup folder (null otherwise).</summary>
    public HeldMod? Held(string id)
    {
        var path = HeldPath(id);
        if (!File.Exists(path)) return null;
        HeldMod? held;
        try { held = JsonSerializer.Deserialize<HeldMod>(File.ReadAllBytes(path)); }
        catch (JsonException) { return null; }
        if (held is null || held.Id != id || held.Objects is null) return null;
        return held.Objects.All(o => File.Exists(Safe(BackupRoot, "objects/" + o))) ? held : null;
    }

    /// <summary>Records the mod's files as held after its uninstall, when the backup folder has the bytes of every mapped file.</summary>
    private void Hold(ModManifest manifest)
    {
        var objects = manifest.Files.Select(f => f.Hash).Distinct(StringComparer.Ordinal).ToList();
        if (!objects.All(o => File.Exists(Safe(BackupRoot, "objects/" + o)))) return; // an older install did not keep every file
        Atomic(HeldPath(manifest.Id), JsonSerializer.SerializeToUtf8Bytes(new HeldMod(manifest.Id, manifest.Digest, objects)));
    }

    /// <summary>The verified files of <paramref name="manifest"/> from the held copy, or null when it is not held in full.</summary>
    private Dictionary<ManifestFile, Func<byte[]>>? FromHeld(ModManifest manifest)
    {
        if (Held(manifest.Id) is not { } held || held.ManifestHash != manifest.Digest) return null;
        return manifest.Files.ToDictionary(f => f, f => (Func<byte[]>)(() => ObjectBytes(f.Hash)));
    }

    /// <summary>
    /// Deletes a mod's files kept on this PC: its held record and its archived uninstall records, then every backup object no other
    /// record names. Refused while the mod is installed.
    /// </summary>
    public void Purge(string id, IProgress<string>? progress = null)
    {
        using var gate = Lock();
        if (ReadReceipt(id) is not null) throw new ModException("uninstall the mod first", ModErrors.Unsupported);
        progress?.Report("apply");
        if (File.Exists(HeldPath(id))) RemoveFile(HeldPath(id));
        if (Safe(BackupRoot, id + ".manifest.json") is var legacy && File.Exists(legacy)) RemoveFile(legacy); // its manifest copy names the files
        if (Safe(BackupRoot, "external-" + id + ".json") is var external && File.Exists(external)) RemoveFile(external); // OD-117 checked copies
        foreach (var record in Directory.EnumerateFiles(BackupRoot, "uninstalled-" + id + "-*.json").ToArray()) RemoveFile(record);
        // its archived transactions name the same bytes (another mod's records keep shared objects)
        foreach (var record in Directory.EnumerateFiles(BackupRoot, "transaction-*.json").ToArray())
        {
            try
            {
                using var journal = JsonDocument.Parse(File.ReadAllBytes(record));
                if (journal.RootElement.TryGetProperty("ReceiptName", out var name) && name.GetString() == id + ".receipt.json") RemoveFile(record);
            }
            catch (JsonException) { } // an unreadable archive is left to retention
        }
        RemoveUnreferenced();
    }
}
