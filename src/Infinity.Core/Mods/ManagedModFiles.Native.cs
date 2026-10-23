// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Mods;

public sealed partial class ManagedModFiles
{
    public bool CanAdopt(ModManifest manifest) => manifest.SupportedForInstall && manifest.Files.All(f => f.Root == "azimuth")
        && manifest.Files.Any(f => f.Role == "binary") && Receipt(manifest) is null
        && manifest.Files.Where(f => f.Mode != "seed").All(f => Current(Target(f.Root, f.Destination)) == f.Hash);

    /// <summary>Explicitly take ownership of a verified standalone installation; never infer game-asset predecessors.</summary>
    public void Adopt(ModManifest manifest)
    {
        using var gate = Lock();
        if (!CanAdopt(manifest)) throw new ModException("existing files cannot be adopted");
        CheckDependencies(manifest);
        var files = new List<ManagedModFile>(); var changes = new List<ModFileChange>();
        foreach (var file in manifest.Files)
        {
            var path = Target(file.Root, file.Destination);
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
        RejectOverlaps(files);
        Atomic(Path.Combine(BackupRoot, manifest.Id + ".manifest.json"), manifest.Snapshot());
        Execute(new(manifest.Id + ".receipt.json", new(manifest.Id, manifest.Digest, true, files, null, _azimuth), changes), null);
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
        if (!visiting.Add(manifest.Id)) throw new ModException("cyclic mod dependencies");
        var installed = InstalledManifests();
        foreach (var dependency in manifest.Document.GetProperty("dependencies").EnumerateArray())
        {
            var id = dependency.GetProperty("id").GetString();
            var target = installed.SingleOrDefault(m => m.Id == id) ?? throw new ModException("required dependency is not managed");
            if (!dependency.GetProperty("versions").EnumerateArray().Any(v => v.GetString() == target.Version)
                || Receipt(target) is not { Enabled: true } || !Verified(target)) throw new ModException("required dependency is disabled or incompatible");
            CheckDependencies(target, visiting);
            if (dependency.TryGetProperty("settings", out var settings) && settings.GetArrayLength() > 0)
            {
                var values = ReadSettings(target).Values;
                foreach (var setting in settings.EnumerateArray())
                    if (!values.TryGetValue(setting.GetProperty("optionId").GetString()!, out var value)
                        || !JsonElement.DeepEquals(value, setting.GetProperty("value"))) throw new ModException("required dependency settings differ");
            }
        }
        if (manifest.Document.TryGetProperty("conflicts", out var conflicts))
            foreach (var conflict in conflicts.EnumerateArray())
                if (installed.Any(m => m.Id == conflict.GetString() && Receipt(m) is { Enabled: true })) throw new ModException("conflicting mod enabled");
        if (manifest.Document.TryGetProperty("incompatibleFiles", out var files))
            foreach (var file in files.EnumerateArray())
                if (File.Exists(Target(file.GetProperty("root").GetString()!, file.GetProperty("path").GetString()!))) throw new ModException("incompatible unmanaged file present");
        foreach (var other in installed.Where(m => Receipt(m) is { Enabled: true }))
            if (other.Document.TryGetProperty("conflicts", out var reverse) && reverse.EnumerateArray().Any(c => c.GetString() == manifest.Id))
                throw new ModException("an enabled mod conflicts with this mod");
        visiting.Remove(manifest.Id);
    }

    public void CheckDependents(string id)
    {
        foreach (var manifest in InstalledManifests())
            if (Receipt(manifest) is { Enabled: true } && manifest.Dependencies.Any(d => d.Id == id))
                throw new ModException("disable dependent mods first");
    }

    public IReadOnlyList<ModManifest> InstalledManifests()
    {
        if (!Directory.Exists(BackupRoot)) return [];
        return Directory.EnumerateFiles(BackupRoot, "*.receipt.json").Select(path =>
        {
            RejectLinks(path);
            var receipt = JsonSerializer.Deserialize<ManagedModReceipt>(File.ReadAllBytes(path)) ?? throw new ModException("invalid receipt");
            var manifest = new ModManifest(File.ReadAllBytes(Safe(BackupRoot, receipt.Id + ".manifest.json")));
            if (!manifest.SupportedForInstall) throw new ModException("installed manifest uses unsupported capabilities");
            _ = Receipt(manifest);
            return manifest;
        }).ToArray();
    }

    public ManifestIni ReadSettings(ModManifest manifest) => new(manifest, File.ReadAllBytes(Target("azimuth", manifest.ConfigPath ?? throw new ModException("settings missing"))));

    public void SaveSettings(ModManifest manifest, ManifestIni settings)
    {
        using var gate = Lock();
        if (settings.ManifestDigest != manifest.Digest) throw new ModException("settings belong to another manifest");
        if (manifest.ConfigPath is null) throw new ModException("mod has no settings");
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
                            || !JsonElement.DeepEquals(value, setting.GetProperty("value"))) throw new ModException("enabled dependent requires this setting");
        Store(File.ReadAllBytes(config));
        var changes = new List<ModFileChange> { new("azimuth", manifest.ConfigPath!, settings.SourceHash, Store(bytes)) };
        Execute(new(manifest.Id + ".receipt.json", receipt, changes), null);
    }
}
