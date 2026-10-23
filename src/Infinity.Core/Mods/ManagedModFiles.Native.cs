// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>What a launch needs from mod state, gathered off the UI thread; warnings never block Play.</summary>
/// <param name="HasEnabledNative">An enabled mod needs the native runtime: a native mod or a data mod with SQL (FIMOD004).</param>
public sealed record ModLaunchPreparation(IReadOnlyList<string> DllOverrides, bool HasEnabledNative, IReadOnlyList<string> Warnings)
{
    /// <summary>An enabled data mod carries SQL for the runtime (FIMOD004): the runtime and hook must support data-mod SQL.</summary>
    public bool HasEnabledDataSql { get; init; }
    public static ModLaunchPreparation None { get; } = new([], false, []);
    /// <summary>Enabled mods that affect multiplayer (<see cref="ModMultiplayer.AffectsMultiplayer"/>): what a Hardcore launch refuses.</summary>
    public IReadOnlyList<ModManifest> MultiplayerMods { get; init; } = [];
}

/// <summary>A Hardcore launch with enabled mods that affect multiplayer; nothing was changed.</summary>
public sealed class ModHardcoreException(IReadOnlyList<ModManifest> mods) : Exception("Hardcore profiles cannot use mods that affect multiplayer")
{
    public IReadOnlyList<ModManifest> Mods { get; } = mods;
}

public sealed partial class ManagedModFiles
{
    /// <summary>
    /// An existing installation matching the manifest exactly: every binary live with its hash, or
    /// (legacy launchers, <c>adoption.parkedFiles</c>) every binary parked with its hash and absent live.
    /// </summary>
    public bool CanAdopt(ModManifest manifest) => AdoptionState(manifest) is not null;

    private bool? AdoptionState(ModManifest manifest)
    {
        if (!manifest.SupportedForInstall || !manifest.Files.All(f => f.Root == "azimuth") || !manifest.Files.Any(f => f.Role == "binary")
            || ReadReceipt(manifest.Id) is not null) return null;
        var mapped = manifest.Files.Where(f => f.Mode != "seed").ToArray();
        // Adoption records no original; a mapping that replaces one cannot be adopted without losing it.
        if (mapped.Any(f => f.Mode == "replace")) return null;
        if (mapped.All(f => Current(Target(f.Root, f.Destination)) == f.Hash)) return true;
        var parked = manifest.ParkedFiles;
        if (parked.Count == 0) return null;
        foreach (var file in mapped)
        {
            var live = Current(Target(file.Root, file.Destination));
            if (file.Role == "binary")
            {
                if (live is not null || !parked.TryGetValue(file.Destination, out var path) || Current(Parked(path)) != file.Hash) return null;
            }
            else if (live is not null && live != file.Hash) return null;
        }
        return false;
    }

    /// <summary>A parked legacy copy: read-only, inside the Azimuth folder, never written or deleted.</summary>
    private string Parked(string relative) => Safe(_azimuth, relative);

    public void Adopt(ModManifest manifest, bool hardcore = false)
    {
        using var gate = Lock();
        var enabled = AdoptionState(manifest) ?? throw new ModException("existing files cannot be adopted", ModErrors.Conflict);
        if (enabled) CheckProfile(manifest, hardcore);
        if (manifest.IsCustom) RequireCustomConfirmation(manifest);
        if (enabled) CheckDependencies(manifest);
        var files = new List<ManagedModFile>(); var changes = new List<ModFileChange>();
        foreach (var file in manifest.Files)
        {
            var path = Target(file.Root, file.Destination);
            if (!enabled && file.Role == "binary")
            {
                // OD-45 parked binary: its bytes become the enabled state; the parked copy stays where it is.
                files.Add(new(file.Root, file.Destination, null, Store(File.ReadAllBytes(Parked(manifest.ParkedFiles[file.Destination]))), false));
                continue;
            }
            if (!File.Exists(path)) continue;
            var hash = Store(File.ReadAllBytes(path));
            files.Add(new(file.Root, file.Destination, file.Role == "binary" ? null : hash, hash, file.Role == "config" || file.Mode == "seed"));
        }
        if (manifest.ConfigPath is { } config)
        {
            var path = Target("azimuth", config); var before = Current(path);
            var ini = new ManifestIni(manifest, before is null ? ManifestIni.Defaults(manifest) : File.ReadAllBytes(path));
            ini.EnforceLocks(); var after = Store(ini.ToBytes());
            if (before is not null) Store(File.ReadAllBytes(path));
            if (before != after) changes.Add(new("azimuth", config, before, after));
            if (!files.Any(f => f.Path == config)) files.Add(new("azimuth", config, before, after, true));
        }
        RejectOverlaps(files, manifest.Id);
        SaveSnapshot(manifest);
        Execute(new(manifest.Id + ".receipt.json", new(manifest.Id, manifest.Digest, enabled, files, null, _azimuth), changes), null);
        // The earlier launcher set this mod's Wine overrides itself: they are the launcher's to remove when the mod is off.
        AdoptWineOverrides(manifest.WineDllOverrides);
        ReconcileWineOverrides();
    }

    private static IEnumerable<(string Name, JsonElement Spec)> PackageSpecs(ModManifest manifest)
    {
        yield return ("primary", manifest.Document.GetProperty("package"));
        if (manifest.Document.TryGetProperty("additionalPackages", out var packages))
            foreach (var package in packages.EnumerateObject()) yield return (package.Name, package.Value);
    }

    public void CheckDependencies(ModManifest manifest) => CheckDependencies(manifest, new HashSet<string>(StringComparer.OrdinalIgnoreCase));

    private void CheckDependencies(ModManifest manifest, HashSet<string> visiting)
    {
        if (!visiting.Add(manifest.Id)) throw new ModException("cyclic mod dependencies", ModErrors.Dependency);
        var installed = InstalledManifests();
        foreach (var dependency in manifest.Document.GetProperty("dependencies").EnumerateArray())
        {
            var id = dependency.GetProperty("id").GetString();
            var target = installed.SingleOrDefault(m => m.Id == id) ?? throw new ModException("required dependency is not managed", ModErrors.Dependency);
            if (!dependency.GetProperty("versions").EnumerateArray().Any(v => v.GetString() == target.Version)
                || Receipt(target) is not { Enabled: true } || !Verified(target)) throw new ModException("required dependency is disabled or incompatible", ModErrors.Dependency);
            CheckDependencies(target, visiting);
            if (dependency.TryGetProperty("settings", out var settings) && settings.GetArrayLength() > 0)
            {
                var values = ReadSettings(target).Values;
                foreach (var setting in settings.EnumerateArray())
                    if (!values.TryGetValue(setting.GetProperty("optionId").GetString()!, out var value)
                        || !JsonElement.DeepEquals(value, setting.GetProperty("value"))) throw new ModException("required dependency settings differ", ModErrors.Dependency);
            }
        }
        if (manifest.Document.TryGetProperty("conflicts", out var conflicts))
            foreach (var conflict in conflicts.EnumerateArray())
                if (installed.Any(m => m.Id == conflict.GetString() && Receipt(m) is { Enabled: true })) throw new ModException("conflicting mod enabled", ModErrors.Conflict);
        if (manifest.Document.TryGetProperty("incompatibleFiles", out var files))
            foreach (var file in files.EnumerateArray())
                if (File.Exists(Target(file.GetProperty("root").GetString()!, file.GetProperty("path").GetString()!))) throw new ModException("incompatible unmanaged file present", ModErrors.Conflict);
        foreach (var other in installed.Where(m => m.Id != manifest.Id && Receipt(m) is { Enabled: true }))
            if (other.Document.TryGetProperty("conflicts", out var reverse) && reverse.EnumerateArray().Any(c => c.GetString() == manifest.Id))
                throw new ModException("an enabled mod conflicts with this mod", ModErrors.Conflict);
        visiting.Remove(manifest.Id);
    }

    /// <summary>Enabled mods depending on <paramref name="id"/> block its removal; with <paramref name="replacement"/>, only those that do not accept that version.</summary>
    public void CheckDependents(string id, ModManifest? replacement = null)
    {
        foreach (var manifest in InstalledManifests())
            if (Receipt(manifest) is { Enabled: true } && manifest.Dependencies.FirstOrDefault(d => d.Id == id) is { } dependency
                && (replacement is null || !dependency.Versions.Contains(replacement.Version)))
                throw new ModException("disable dependent mods first", ModErrors.Dependency);
    }

    public IReadOnlyList<ModManifest> InstalledManifests()
    {
        if (!Directory.Exists(BackupRoot)) return [];
        return Directory.EnumerateFiles(BackupRoot, "*.receipt.json").Select(path =>
        {
            RejectLinks(path);
            var receipt = JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path)) ?? throw new ModException("invalid receipt", ModErrors.Recovery);
            var manifest = SnapshotFor(receipt);
            if (!manifest.SupportedForInstall) throw new ModException("installed manifest uses unsupported capabilities", ModErrors.Unsupported);
            _ = Receipt(manifest);
            return manifest;
        }).ToArray();
    }

    public ManifestIni ReadSettings(ModManifest manifest) => new(manifest, File.ReadAllBytes(Target("azimuth", manifest.ConfigPath ?? throw new ModException("settings missing", ModErrors.Settings))));

    public void SaveSettings(ModManifest manifest, ManifestIni settings)
    {
        ArgumentNullException.ThrowIfNull(settings);
        using var gate = Lock();
        if (settings.ManifestDigest != manifest.Digest) throw new ModException("settings belong to another manifest", ModErrors.Settings);
        if (manifest.ConfigPath is null) throw new ModException("mod has no settings", ModErrors.Settings);
        var receipt = Receipt(manifest) ?? throw new ModException("mod is not installed");
        var config = Target("azimuth", manifest.ConfigPath!);
        if (Current(config) != settings.SourceHash) throw new ModSettingsChangedException();
        if (receipt.Enabled) CheckDependencies(manifest);
        settings.EnforceLocks();
        var bytes = settings.ToBytes();
        var parsed = new ManifestIni(manifest, bytes);
        foreach (var dependent in InstalledManifests().Where(m => Receipt(m) is { Enabled: true }))
            foreach (var dependency in dependent.Document.GetProperty("dependencies").EnumerateArray().Where(d => d.GetProperty("id").GetString() == manifest.Id))
                if (dependency.TryGetProperty("settings", out var required))
                    foreach (var setting in required.EnumerateArray())
                        if (!parsed.Values.TryGetValue(setting.GetProperty("optionId").GetString()!, out var value)
                            || !JsonElement.DeepEquals(value, setting.GetProperty("value"))) throw new ModException("enabled dependent requires this setting", ModErrors.Dependency);
        Store(File.ReadAllBytes(config));
        var changes = new List<ModFileChange> { new("azimuth", manifest.ConfigPath!, settings.SourceHash, Store(bytes)) };
        Execute(new(manifest.Id + ".receipt.json", receipt, changes), null);
    }

    /// <summary>M6: out-of-schema values back to their defaults, later duplicate keys commented out; nothing else changes.</summary>
    public void RepairSettings(ModManifest manifest)
    {
        var settings = ReadSettings(manifest);
        if (settings.Issues.Count == 0) return;
        settings.Repair();
        SaveSettings(manifest, settings);
    }

    /// <summary>
    /// Before every launch, off the UI thread: locked settings of enabled mods are written back
    /// (journaled, OD-45/§1.3) and Wine overrides reconciled. No receipts means nothing is read or
    /// written. Drift and settings issues are warnings; only an interrupted operation of this Azimuth
    /// folder throws (<see cref="ModErrors.Recovery"/>).
    /// </summary>
    public ModLaunchPreparation PrepareForLaunch(bool hardcore = false)
    {
        if (!Directory.Exists(BackupRoot) || !RecoveryRequiredHere && !Directory.EnumerateFiles(BackupRoot, "*.receipt.json").Any()) return ModLaunchPreparation.None;
        if (RecoveryRequiredHere) throw new ModException("interrupted mod operation needs recovery", ModErrors.Recovery);
        if (hardcore)
        {
            // OD-112 follow-up: refused before anything is written; the files stay exactly as they are.
            var refused = new List<ModManifest>();
            foreach (var path in Directory.EnumerateFiles(BackupRoot, "*.receipt.json").ToArray())
            {
                try
                {
                    var receipt = ReadReceipt(Path.GetFileName(path)[..^".receipt.json".Length]);
                    if (receipt is { Enabled: true } && SnapshotFor(receipt) is var manifest && ModMultiplayer.AffectsMultiplayer(manifest)) refused.Add(manifest);
                }
                catch (Exception error) when (IsStateError(error)) { }
            }
            if (refused.Count > 0) throw new ModHardcoreException(refused);
        }
        var warnings = new List<string>();
        var enabled = new List<ModManifest>();
        using (var gate = Lock(allowRecovery: true))
        {
            foreach (var path in Directory.EnumerateFiles(BackupRoot, "*.receipt.json").ToArray())
            {
                var id = Path.GetFileName(path)[..^".receipt.json".Length];
                ManagedModReceipt receipt; ModManifest manifest;
                try
                {
                    receipt = ReadReceipt(id) ?? throw new ModException("receipt vanished", ModErrors.Recovery);
                    manifest = SnapshotFor(receipt);
                }
                catch (Exception error) when (IsStateError(error)) { warnings.Add(id + ": unreadable " + Code(error)); continue; } // shown on Mods, never a Play blocker
                if (!receipt.Enabled) continue;
                enabled.Add(manifest);
                if (DriftOf(receipt).Count > 0) warnings.Add(manifest.Id + ": drift");
                if (manifest.ConfigPath is not { } config || !File.Exists(Target("azimuth", config))) continue;
                var settings = ReadSettings(manifest);
                if (settings.Issues.Count > 0) warnings.Add(manifest.Id + ": settings");
                var before = settings.ToBytes(); settings.EnforceLocks(); var after = settings.ToBytes();
                if (before.SequenceEqual(after)) continue;
                Store(before);
                Execute(new(manifest.Id + ".receipt.json", receipt, [new("azimuth", config, settings.SourceHash, Store(after))]), null);
            }
        }
        var multiplayer = enabled.Where(ModMultiplayer.AffectsMultiplayer).ToArray();
        if (warnings.Any(w => w.Contains(": unreadable", StringComparison.Ordinal)))
            return new(enabled.SelectMany(m => m.WineDllOverrides.Select(w => w.Module)).Distinct(StringComparer.OrdinalIgnoreCase).ToArray(), enabled.Any(m => m.NeedsRuntime), warnings) { MultiplayerMods = multiplayer, HasEnabledDataSql = enabled.Any(m => m.HasDatabaseSql || m.DatabaseBase is not null) };
        ReconcileWineOverrides();
        return new(NativeDllOverrides(), enabled.Any(m => m.NeedsRuntime), warnings) { MultiplayerMods = multiplayer, HasEnabledDataSql = enabled.Any(m => m.HasDatabaseSql || m.DatabaseBase is not null) };
    }
}
