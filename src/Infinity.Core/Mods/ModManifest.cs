// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Security.Cryptography;
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>
/// OD-232: a mod the launcher installs is an ExtraOptions mod: every file under <c>&lt;Azimuth&gt;\mods\&lt;game folder&gt;\&lt;id&gt;\</c>
/// (OD-233; <c>mods/&lt;id&gt;/</c> destinations of the old flat layout are read as their game's, <see cref="ModsGame"/>), its own
/// <c>mod.json</c> among them (plugins included), from a pinned download or the player's own files (OD-117). Every other format
/// (the Infinity native runtime, game-folder and media-override files, whole-database replacement, startup SQL, the
/// <c>extraOptionsAddon</c> conversion, Infinity's own custom mods) is refused from a catalog like any invalid manifest; an
/// installed manifest that used one (<see cref="Installed"/>) is read with <see cref="RetiredFeatures"/>, so that mod stays
/// listed and removable but is never installed, updated or enabled again.
/// </summary>
public sealed class ModManifest
{
    private readonly byte[] _bytes;
    public JsonElement Document { get; }
    public string Id => Document.GetProperty("id").GetString()!;
    public string Version => Document.GetProperty("version").GetString()!;
    public string Digest { get; }
    /// <summary>OD-125/127: part of Infinity (only ExtraOptions.Infinity, from the launcher itself): options only, nothing to install.</summary>
    public bool IsBuiltIn => Document.TryGetProperty("builtIn", out var builtIn) && builtIn.ValueKind == JsonValueKind.True;
    public string PackageUrl => Document.GetProperty("package").GetProperty("url").GetString()!;
    public string PackageHash => Document.GetProperty("package").GetProperty("sha256").GetString()!;
    public long PackageBytes => Document.GetProperty("package").GetProperty("bytes").GetInt64();
    public IReadOnlyList<ManifestFile> Files { get; }
    public IReadOnlyList<ManifestDependency> Dependencies { get; }
    /// <summary>OD-206 (W11): the server game ids the mod is for (<c>games</c>); a manifest that declares none is FH2's, as every mod was before.</summary>
    public IReadOnlyList<string> Games { get; }
    public const string DefaultGame = "fh2";
    /// <summary>True when the mod is offered for the game with this id.</summary>
    public bool IsForGame(string gameId) => Games.Contains(gameId, StringComparer.Ordinal);
    /// <summary>Wine/Proton native-first overrides applied while the mod is enabled (launch.wineDllOverrides).</summary>
    public IReadOnlyList<ModWineOverride> WineDllOverrides { get; }
    /// <summary>Legacy parked copies (adoption.parkedFiles): mapped binary destination to its parked Azimuth-relative path.</summary>
    public IReadOnlyDictionary<string, string> ParkedFiles { get; }
    public ModMultiplayer? Multiplayer { get; }
    /// <summary>The mod's settings file, Azimuth-relative; a path of the old flat layout is read as its game's (OD-233, as <see cref="Files"/>).</summary>
    public string? ConfigPath { get; }

    /// <summary>
    /// OD-233: the game whose mods folder (<c>mods\&lt;game folder&gt;</c>, <see cref="ExtraOptionsLayout"/>) the mod installs into: its one
    /// game, or, for a mod that declares several, the one its destinations name. Destinations written for the old flat layout
    /// (<c>mods/&lt;id&gt;/…</c>, every catalog entry before OD-233) are read as that game's folder (<see cref="ExtraOptionsLayout.CanonicalDestination"/>):
    /// <see cref="Files"/> and <see cref="ConfigPath"/> always carry the per-game path, so an installation and an update see the same files
    /// whichever form the catalog used. <see cref="Digest"/> stays the digest of the manifest's own bytes.
    /// </summary>
    public string ModsGame { get; }

    /// <summary>
    /// OD-232: what an installed manifest uses that this launcher no longer supports (the manifest field, or <c>infinity-native</c>,
    /// <c>game-files</c>, <c>custom</c>, <c>loader</c>, <c>outside-mod-folder</c>, <c>mod-json</c>); empty for every manifest it accepts.
    /// </summary>
    public IReadOnlyList<string> RetiredFeatures { get; }
    /// <summary>OD-232: an installed mod in a format the launcher no longer supports: listed and removable only.</summary>
    public bool Retired => RetiredFeatures.Count > 0;
    public bool SupportedForInstall => !Retired && Document.TryGetProperty("runtime", out var value) && value.GetString() == "standalone";

    /// <summary>
    /// OD-232 (<c>install.loadOrder: "first"</c>, the ZephyrSkies Commons): the launcher keeps this mod's folder at the top of
    /// <c>loadorder.txt</c> (loaded first, lowest priority), ahead of every other mod; see <see cref="ExtraOptionsMods.ArrangeLoadOrder"/>.
    /// </summary>
    public bool LoadFirst { get; }

    /// <summary>
    /// OD-236 (Alex 2026-11-10: "Hardcore with car packs. These mods are not counting as modded. Players can use spotify-radio, carpacks,
    /// garage-revving on HARDCORE"): the manifest's own <c>countsAsModded</c> (a JSON boolean; anything else is refused), or null when it
    /// declares none. A mod declared <c>false</c> never makes a session <c>modded</c> and is the only kind a Hardcore profile keeps on
    /// (<see cref="ModMultiplayer.CountsAsModded"/>, where a catalog entry's declaration stands for an installed copy of the same id).
    /// </summary>
    public bool? CountsAsModdedDeclared { get; }

    /// <summary>
    /// Whether this manifest makes a session <c>modded</c>: its <see cref="CountsAsModdedDeclared"/>; undeclared, true, except (until the
    /// official catalog carries the field, OD-236) an official car pack (<see cref="IsCarPack"/>) or a mod whose multiplayer impact is
    /// cosmetic (OD-233's "both modes" rule, <see cref="ModMultiplayer.AffectsMultiplayer"/>), which kept their place in Hardcore before it.
    /// </summary>
    public bool CountsAsModded => CountsAsModdedDeclared ?? !(IsCarPack || !ModMultiplayer.AffectsMultiplayer(this));

    /// <summary>
    /// The transitional rule of <see cref="CountsAsModded"/> for an undeclared manifest: an official car pack (OD-231/OD-232, the ZephyrSkies
    /// packs and their Commons) is a catalog mod made of the player's own files (<see cref="IsExternal"/>) that carries content only
    /// (database, strings, media: no binary, no plugin, nothing executable).
    /// </summary>
    public bool IsCarPack => IsExternal && Files.Count > 0 && Files.All(f => f.Role != "binary" && !ExecutableExtension(f.Destination)
        && !f.Destination.Contains("/plugins/", StringComparison.OrdinalIgnoreCase));

    /// <summary>The manifest field of <see cref="CountsAsModdedDeclared"/>.</summary>
    public const string CountsAsModdedField = "countsAsModded";

    /// <summary>A manifest from a catalog or a development file: anything that is not an ExtraOptions mod is refused (OD-232).</summary>
    public ModManifest(byte[] bytes) : this(bytes, installed: false) { }

    /// <summary>
    /// OD-232: an installed mod's manifest snapshot. A retired feature is recorded in <see cref="RetiredFeatures"/> instead of refused
    /// (its own rules are no longer checked), so the mod is still listed and its uninstall restores what its receipt names.
    /// </summary>
    internal static ModManifest Installed(byte[] bytes) => new(bytes, installed: true);

    private ModManifest(byte[] bytes, bool installed)
    {
        ArgumentNullException.ThrowIfNull(bytes);
        if (bytes.Length > 256 * 1024) throw new ModException("manifest too large");
        _bytes = (byte[])bytes.Clone();
        using var json = JsonDocument.Parse(_bytes, new JsonDocumentOptions { MaxDepth = 32 });
        RejectDuplicates(json.RootElement);
        Document = json.RootElement.Clone();
        if (Document.GetProperty("schemaVersion").GetInt32() != 1) throw new ModException("unsupported manifest");
        var retired = new List<string>();
        void Retire(string feature)
        {
            if (!installed) throw new ModException("OD-232: the launcher installs only ExtraOptions mods (" + feature + ")", ModErrors.Unsupported);
            if (!retired.Contains(feature, StringComparer.Ordinal)) retired.Add(feature);
        }
        foreach (var field in new[] { "name", "description" })
        {
            var localized = Document.GetProperty(field);
            if (localized.ValueKind != JsonValueKind.Object) throw new ModException("invalid localized text");
            foreach (var value in localized.EnumerateObject()) ValidateText(value.Value);
            ValidateText(localized.GetProperty("en"));
        }
        foreach (var field in new[] { "version", "author", "license" }) ValidateText(Document.GetProperty(field));
        foreach (var version in Document.GetProperty("compatibility").GetProperty("azimuthVersions").EnumerateArray()) ValidateText(version);
        // OD-114's custom mods (custom.<name>, a local files/ payload) were retired by OD-232; only their installations are still read.
        var legacyCustom = LegacyCustomId(Id);
        if (legacyCustom) Retire("custom");
        else if (!ValidId(Id)) throw new ModException("invalid mod id");
        if (Document.TryGetProperty("runtime", out var runtime) && runtime.ValueKind == JsonValueKind.String && runtime.GetString() == "infinity-native"
            || Document.TryGetProperty("nativeHost", out _)) Retire("infinity-native");
        foreach (var field in new[] { "database", "databaseSql", "databaseBase", "extraOptionsAddon", "readiness" })
            if (Document.TryGetProperty(field, out _)) Retire(field);
        foreach (var field in new[] { "databaseMigrations", "runtimeOutputs" })
            if (Document.GetProperty(field) is var list && (list.ValueKind != JsonValueKind.Array || list.GetArrayLength() > 0)) Retire(field);
        if (Document.TryGetProperty("payload", out _)) Retire("custom");
        Games = Document.TryGetProperty("games", out var games) ? ParseGames(games) : [DefaultGame];
        ModsGame = ModsGameOf(Document, Games, IsBuiltIn);
        // OD-232: an ExtraOptions mod lives only in its own folder; ExtraOptions.Infinity's options file sits beside azimuth.exe.
        // OD-233: that folder is in its game's mods folder; an old flat-layout path is read as it.
        var folder = ExtraOptionsMods.FolderPrefix(ModsGame, Id);
        ExternalSources = ExternalSourcesOf(Document);
        ConfigPath = Document.GetProperty("config") is { ValueKind: JsonValueKind.Object } configNode && configNode.GetProperty("path").GetString() is { } rawConfig
            ? ExtraOptionsLayout.CanonicalDestination(rawConfig, ModsGame) : null;
        if (ConfigPath is { } configPath)
        {
            ValidatePath(configPath);
            if (!configPath.EndsWith(".ini", StringComparison.OrdinalIgnoreCase)) throw new ModException("INI configuration must use an ini file");
            if (!IsBuiltIn && !configPath.StartsWith(folder, StringComparison.Ordinal)) Retire("outside-mod-folder");
            var config = Document.GetProperty("config");
            if (config.TryGetProperty("root", out var configRoot) && configRoot.GetString() != "azimuth"
                || config.TryGetProperty("format", out var configFormat) && configFormat.GetString() != "ini") throw new ModException("unsupported configuration format");
            if (config.TryGetProperty("options", out var options))
            {
                if (options.GetArrayLength() > 128) throw new ModException("too many settings");
                var ids = new HashSet<string>(StringComparer.Ordinal); var bindings = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
                foreach (var option in options.EnumerateArray())
                {
                    var id = option.GetProperty("id").GetString()!; var section = option.GetProperty("section").GetString()!; var key = option.GetProperty("key").GetString()!;
                    if (!ids.Add(id) || !bindings.Add(section + "\0" + key) || section.Length is < 1 or > 128 || key.Length is < 1 or > 128
                        || section.IndexOfAny(['\r','\n',']','[','\0']) >= 0 || key.IndexOfAny(['\r','\n','=','\0']) >= 0) throw new ModException("invalid setting binding");
                    if (option.GetProperty("type").GetString() == "enum")
                    {
                        var choices = option.GetProperty("choices").EnumerateArray().Select(c => c.GetProperty("value").GetString()!).ToArray();
                        if (choices.Length is < 1 or > 64 || choices.Distinct(StringComparer.Ordinal).Count() != choices.Length
                            || choices.Any(v => v.Length is < 1 or > 64 || v.Any(c => !char.IsAsciiLetterOrDigit(c) && c is not ('_' or '-'))))
                            throw new ModException("invalid enum token");
                    }
                    ModMultiplayer.ValidateValue(option, option.GetProperty("default"));
                    if (option.TryGetProperty("lockedValue", out var locked)) ModMultiplayer.ValidateValue(option, locked);
                    foreach (var field in new[] { "label", "description", "lockReason" })
                        if (option.TryGetProperty(field, out var text))
                        {
                            if (text.ValueKind != JsonValueKind.Object) throw new ModException("invalid localized text");
                            foreach (var value in text.EnumerateObject()) ValidateText(value.Value);
                        }
                    if (option.TryGetProperty("lockReason", out _) && !option.TryGetProperty("lockedValue", out _)) throw new ModException("lock reason without a locked value");
                }
            }
        }
        Digest = Hash(_bytes);
        var files = new List<ManifestFile>();
        foreach (var root in new[] { "game", "azimuth" })
            foreach (var file in Document.GetProperty("files").GetProperty(root).EnumerateArray())
            {
                var destination = file.GetProperty("destination").GetString()!;
                if (root == "azimuth") destination = ExtraOptionsLayout.CanonicalDestination(destination, ModsGame);
                var item = new ManifestFile(root, file.GetProperty("source").GetString()!, destination,
                    file.GetProperty("sha256").GetString()!, file.GetProperty("bytes").GetInt64(),
                    file.GetProperty("role").GetString()!, file.GetProperty("mode").GetString()!,
                    file.TryGetProperty("package", out var package) ? package.GetString()! : "primary");
                // OD-115's loader (a proxy DLL the game process loads by itself) and every game-folder file were retired by OD-232.
                if (file.TryGetProperty("loader", out var loader))
                {
                    if (loader.ValueKind is not (JsonValueKind.True or JsonValueKind.False)) throw new ModException("invalid loader flag");
                    if (loader.ValueKind == JsonValueKind.True) Retire("loader");
                }
                if (root == "game") Retire("game-files");
                else if (!item.Destination.StartsWith(folder, StringComparison.Ordinal)) Retire("outside-mod-folder");
                ValidatePath(IsExternal ? ExternalPath(item.Source) : item.Source); ValidatePath(item.Destination);
                // OD-117: the player's own files are data only, never code the game or a loader would run.
                if (IsExternal && (item.Role == "binary" || ExecutableExtension(item.Destination) || ExecutableExtension(item.Source)))
                    throw new ModException("external files carry no executable code", ModErrors.Unsupported);
                if (item.Bytes is < 0 or > 64 * 1024 * 1024 || (item.Hash.Length != 64 || item.Hash.Any(c => !char.IsAsciiHexDigit(c)))
                    || item.Mode is not ("add" or "replace" or "add-or-replace" or "seed")) throw new ModException("invalid file mapping");
                if (item.Role is not ("binary" or "config" or "asset" or "user-asset" or "license")
                    || item.Mode == "seed" && item.Role is not ("config" or "user-asset")
                    || item.Role == "config" && (item.Root != "azimuth" || item.Mode != "seed" || item.Destination != ConfigPath)
                    || item.Root == "azimuth" && item.Destination.Equals(ConfigPath, StringComparison.OrdinalIgnoreCase) && item.Role != "config")
                    throw new ModException("invalid file role or settings mapping");
                files.Add(item);
            }
        if (files.Count > 512 || files.Select(f => f.Root + "/" + f.Destination).Distinct(StringComparer.OrdinalIgnoreCase).Count() != files.Count)
            throw new ModException("duplicate or excessive files");
        Files = files.AsReadOnly();
        // OD-232: ExtraOptions loads the folder by its own mod.json, which the launcher checks against this manifest at install.
        if (!IsBuiltIn && !Files.Any(f => f.Root == "azimuth" && f.Destination == folder + "mod.json")) Retire("mod-json");
        Dependencies = Document.GetProperty("dependencies").EnumerateArray().Select(d => new ManifestDependency(
            d.GetProperty("id").GetString()!, d.GetProperty("versions").EnumerateArray().Select(v => v.GetString()!).ToArray())).ToArray();
        bool Known(string? id) => ValidId(id) || legacyCustom && LegacyCustomId(id);
        if (Dependencies.Count > 64 || Dependencies.Select(d => d.Id).Distinct(StringComparer.OrdinalIgnoreCase).Count() != Dependencies.Count
            || Dependencies.Any(d => d.Id.Equals(Id, StringComparison.OrdinalIgnoreCase) || !Known(d.Id) || d.Versions.Count == 0
                || d.Versions.Any(string.IsNullOrWhiteSpace))) throw new ModException("invalid dependencies");
        Multiplayer = ModMultiplayer.Parse(Document, legacyCustom);
        if (Document.TryGetProperty("conflicts", out var conflicts) && conflicts.EnumerateArray().Any(c => !Known(c.GetString())))
            throw new ModException("invalid conflicts");
        if (IsBuiltIn)
        {
            // OD-125/127: ExtraOptions.Infinity comes with Infinity's hook; its manifest (embedded in the launcher) only describes its options.
            if (Id != ManagedModFiles.BuiltInExtraOptions || IsExternal || Files.Count > 0 || Document.TryGetProperty("package", out _)
                || Document.TryGetProperty("additionalPackages", out _)) throw new ModException("invalid built-in mod", ModErrors.Unsupported);
        }
        else if (IsExternal)
        {
            // OD-117: no download at all; every byte comes from the player's sources, checked against these pins.
            if (Document.TryGetProperty("package", out _) || Document.TryGetProperty("additionalPackages", out _))
                throw new ModException("a mod of external files has no download", ModErrors.Unsupported);
        }
        else if (!legacyCustom) ValidatePackage(Document.GetProperty("package"));
        var packageNames = new HashSet<string>(StringComparer.Ordinal) { "primary" };
        if (Document.TryGetProperty("additionalPackages", out var additional))
            foreach (var p in additional.EnumerateObject())
            {
                if (!packageNames.Add(p.Name) || packageNames.Count > 16) throw new ModException("invalid additional package");
                ValidatePackage(p.Value);
            }
        if (Files.Any(f => !packageNames.Contains(f.Package))) throw new ModException("unknown package mapping");
        var wine = new List<ModWineOverride>();
        if (Document.TryGetProperty("launch", out var launch) && launch.TryGetProperty("wineDllOverrides", out var overrides))
            foreach (var item in overrides.EnumerateArray())
            {
                // A bare module name is shorthand for { "module": name, "appDefaults": true }.
                var name = item.ValueKind == JsonValueKind.String ? item.GetString()! : item.GetProperty("module").GetString()!;
                var appDefaults = item.ValueKind != JsonValueKind.Object || !item.TryGetProperty("appDefaults", out var flag) || flag.GetBoolean();
                if (item.ValueKind == JsonValueKind.Object && item.EnumerateObject().Any(p => p.Name is not ("module" or "appDefaults")))
                    throw new ModException("unsupported DLL override field");
                if (name.Length is < 1 or > 64 || name.Any(c => !char.IsAsciiLetterOrDigit(c) && c != '_' && c != '-')
                    || wine.Any(w => w.Module.Equals(name, StringComparison.OrdinalIgnoreCase))
                    || !Files.Any(f => f.Root == "azimuth" && f.Role == "binary" && f.Destination.Equals(name + ".dll", StringComparison.OrdinalIgnoreCase)))
                    throw new ModException("DLL override must name a mapped binary");
                wine.Add(new(name, appDefaults));
            }
        WineDllOverrides = wine.AsReadOnly();
        var parked = new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
        if (Document.TryGetProperty("adoption", out var adoption) && adoption.TryGetProperty("parkedFiles", out var parkedFiles))
            foreach (var item in parkedFiles.EnumerateArray())
            {
                var path = item.GetProperty("path").GetString()!;
                var destination = ExtraOptionsLayout.CanonicalDestination(item.GetProperty("destination").GetString()!, ModsGame);
                ValidatePath(path);
                if (parked.Count >= 16 || !Files.Any(f => f.Root == "azimuth" && f.Role == "binary" && f.Destination == destination) || !parked.TryAdd(destination, path))
                    throw new ModException("parked legacy files must name a mapped azimuth binary");
            }
        ParkedFiles = parked;
        // OD-232: "first" is the only load order a manifest may ask for (anything else is refused like any invalid field).
        if (Document.TryGetProperty("loadOrder", out var loadOrder))
        {
            if (loadOrder.ValueKind != JsonValueKind.String || loadOrder.GetString() != "first") throw new ModException("invalid load order");
            LoadFirst = true;
        }
        // OD-236: a JSON boolean only (a string "false" or a number is refused like any invalid field)
        if (Document.TryGetProperty(CountsAsModdedField, out var counts))
            CountsAsModdedDeclared = counts.ValueKind switch
            {
                JsonValueKind.True => true,
                JsonValueKind.False => false,
                _ => throw new ModException("invalid countsAsModded"),
            };
        if (Document.TryGetProperty("incompatibleFiles", out var incompatible))
            foreach (var file in incompatible.EnumerateArray())
            {
                if (file.GetProperty("root").GetString() is not ("game" or "azimuth")) throw new ModException("invalid incompatible file root");
                ValidatePath(file.GetProperty("path").GetString()!);
            }
        RetiredFeatures = retired.AsReadOnly();
    }

    /// <summary>
    /// OD-117: a catalog mod made of the player's own files (a car pack they already have): <c>externalSources</c> instead of a
    /// download; each mapping's <c>source</c> is <c>&lt;source id&gt;:&lt;path inside it&gt;</c>, pinned by SHA-256 and size.
    /// </summary>
    public bool IsExternal => Document.TryGetProperty("externalSources", out _);
    public IReadOnlyList<ExternalSource> ExternalSources { get; }

    /// <summary>The source id of an external mapping (<c>pack</c> of <c>pack:cars/x.zip</c>).</summary>
    public static string ExternalSourceId(string source) => source.IndexOf(':', StringComparison.Ordinal) is var colon and > 0 ? source[..colon] : "";

    /// <summary>The path inside the source, after a declared source id; anything else is refused.</summary>
    private string ExternalPath(string source)
    {
        var id = ExternalSourceId(source);
        if (id.Length == 0 || ExternalSources.All(s => s.Id != id)) throw new ModException("external mapping names an undeclared source", ModErrors.Unsupported);
        return source[(id.Length + 1)..];
    }

    private static System.Collections.ObjectModel.ReadOnlyCollection<ExternalSource> ExternalSourcesOf(JsonElement document)
    {
        if (!document.TryGetProperty("externalSources", out var list)) return new([]);
        if (list.ValueKind != JsonValueKind.Array || list.GetArrayLength() is < 1 or > 8)
            throw new ModException("invalid external sources", ModErrors.Unsupported);
        var sources = new List<ExternalSource>();
        foreach (var item in list.EnumerateArray())
        {
            var id = item.GetProperty("id").GetString()!; var kind = item.GetProperty("kind").GetString()!;
            if (id.Length is < 1 or > 32 || id.Any(c => c is not (>= 'a' and <= 'z' or >= '0' and <= '9' or '-')) || sources.Any(s => s.Id == id)
                || kind is not ("folder" or "file" or "either")) throw new ModException("invalid external source", ModErrors.Unsupported);
            foreach (var field in new[] { "label", "help" })
            {
                var text = item.GetProperty(field);
                if (text.ValueKind != JsonValueKind.Object) throw new ModException("invalid localized text");
                foreach (var value in text.EnumerateObject()) ValidateText(value.Value);
                ValidateText(text.GetProperty("en"));
            }
            string? hint = null;
            if (item.TryGetProperty("hint", out var hintValue)) { ValidateText(hintValue); hint = hintValue.GetString(); }
            sources.Add(new(id, kind, item.GetProperty("label").Clone(), item.GetProperty("help").Clone(), hint));
        }
        return sources.AsReadOnly();
    }

    private static void ValidatePackage(JsonElement package)
    {
        var hash = package.GetProperty("sha256").GetString()!;
        if (package.GetProperty("bytes").GetInt64() is <= 0 or > 64 * 1024 * 1024 || hash.Length != 64
            || hash.Any(c => !char.IsAsciiHexDigit(c) || char.IsUpper(c))
            || !Uri.TryCreate(package.GetProperty("url").GetString(), UriKind.Absolute, out var url)
            || url.Scheme != "https" || url.UserInfo.Length > 0 || url.Fragment.Length > 0) throw new ModException("invalid package");
    }

    private static void ValidateText(JsonElement value)
    {
        if (value.ValueKind != JsonValueKind.String || value.GetString() is not { Length: > 0 and <= 4096 })
            throw new ModException("invalid manifest text");
    }

    /// <summary>OD-114's reserved prefix of the retired custom mods (OD-232): only an installed one's manifest still carries it.</summary>
    private const string LegacyCustomPrefix = "custom.";

    /// <summary>A retired custom mod's id (<c>custom.</c> + <c>[a-z0-9][a-z0-9-]{0,56}</c>), read only from an installation (OD-232).</summary>
    internal static bool LegacyCustomId(string? id)
    {
        if (id is null || !id.StartsWith(LegacyCustomPrefix, StringComparison.Ordinal)) return false;
        var rest = id[LegacyCustomPrefix.Length..];
        return rest.Length is >= 1 and <= 57 && (char.IsAsciiLetterLower(rest[0]) || char.IsAsciiDigit(rest[0]))
            && rest.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-');
    }

    /// <summary>Mod ids: <c>^[a-z][a-z0-9-]{0,63}$</c>.</summary>
    public static bool ValidId(string? id) =>
        id is { Length: >= 1 and <= 64 } && char.IsAsciiLetterLower(id[0]) && id.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '-');

    private static readonly string[] Executables = [".dll", ".exe", ".asi", ".sys", ".com", ".bat", ".cmd", ".ps1", ".scr", ".msi", ".cpl", ".ocx", ".drv", ".vbs"];
    /// <summary>A file Windows or a loader would execute, by extension.</summary>
    public static bool ExecutableExtension(string path) => Executables.Any(e => path.EndsWith(e, StringComparison.OrdinalIgnoreCase));

    public byte[] Snapshot() => (byte[])_bytes.Clone();

    /// <summary>
    /// OD-116: the same manifest whatever its bytes (property order, spacing, escapes). A manifest rebuilt from a schema-2 catalog
    /// entry is not byte-identical to the one 1.0.9 installed from its separate file; the installed one is kept when they match.
    /// </summary>
    public bool SameContent(ModManifest other)
    {
        if (ReferenceEquals(this, other) || other is not null && Digest == other.Digest) return true;
        if (other is null) return false;
        // A string a writer cannot re-encode (a lone surrogate escape) is never "the same": the bytes decide, conservatively.
        try { return ModsGame == other.ModsGame && Canonical(Document, ModsGame).AsSpan().SequenceEqual(Canonical(other.Document, other.ModsGame)); }
        catch (InvalidOperationException) { return false; }
        catch (ArgumentException) { return false; }
    }

    /// <summary>
    /// OD-233: paths (<c>destination</c>, <c>path</c>) compare in their per-game form, so a catalog that moved to it is no update. The top-level
    /// <c>countsAsModded</c> (OD-236) is left out: it is the catalog's policy, not the mod's files (a pack installed before its entry declared
    /// it is the same pack, and is never asked for again), and <see cref="ModMultiplayer.CountsAsModded"/> reads it from the catalog entry.
    /// </summary>
    private static byte[] Canonical(JsonElement element, string game)
    {
        using var buffer = new MemoryStream();
        using (var writer = new Utf8JsonWriter(buffer)) Write(writer, element, null, root: true);
        return buffer.ToArray();

        void Write(Utf8JsonWriter writer, JsonElement value, string? name, bool root = false)
        {
            switch (value.ValueKind)
            {
                case JsonValueKind.Object:
                    writer.WriteStartObject();
                    foreach (var property in value.EnumerateObject().Where(p => !root || p.Name != CountsAsModdedField).OrderBy(p => p.Name, StringComparer.Ordinal))
                    { writer.WritePropertyName(property.Name); Write(writer, property.Value, property.Name); }
                    writer.WriteEndObject(); break;
                case JsonValueKind.Array:
                    writer.WriteStartArray();
                    foreach (var item in value.EnumerateArray()) Write(writer, item, null);
                    writer.WriteEndArray(); break;
                case JsonValueKind.String when name is "destination" or "path":
                    writer.WriteStringValue(ExtraOptionsLayout.CanonicalDestination(value.GetString()!, game)); break;
                case JsonValueKind.String: writer.WriteStringValue(value.GetString()); break;
                // Numbers keep their exact text (20.0 and 20 are different settings defaults to a reviewer).
                default: value.WriteTo(writer); break;
            }
        }
    }
    public string Text(string field, string locale) => Document.GetProperty(field).TryGetProperty(locale, out var text)
        ? text.GetString()! : Document.GetProperty(field).GetProperty("en").GetString()!;
    /// <summary>
    /// OD-233: a mod's files sit in one game's mods folder: its one game, or, declaring several, the game folder its destinations name (an
    /// old flat-layout path names none). A built-in card installs nothing (its first game).
    /// </summary>
    private static string ModsGameOf(JsonElement document, IReadOnlyList<string> games, bool builtIn)
    {
        if (games.Count == 1 || builtIn) return games[0];
        var named = new HashSet<string>(StringComparer.Ordinal);
        if (document.TryGetProperty("files", out var files) && files.ValueKind == JsonValueKind.Object
            && files.TryGetProperty("azimuth", out var azimuth) && azimuth.ValueKind == JsonValueKind.Array)
            foreach (var file in azimuth.EnumerateArray())
                if (file.ValueKind == JsonValueKind.Object && file.TryGetProperty("destination", out var destination) && destination.ValueKind == JsonValueKind.String
                    && destination.GetString()!.Split('/') is [ExtraOptionsMods.ModsFolder, var folder, _, ..] && ExtraOptionsLayout.GameOfFolder(folder) is { } game)
                    named.Add(game);
        if (named.Count == 1 && games.Contains(named.First(), StringComparer.Ordinal)) return named.First();
        throw new ModException("a mod for several games installs into one game's mods folder, named by its destinations", ModErrors.Unsupported);
    }

    private static string[] ParseGames(JsonElement games)
    {
        if (games.ValueKind != JsonValueKind.Array) throw new ModException("invalid games");
        var ids = games.EnumerateArray().Select(g => g.ValueKind == JsonValueKind.String ? g.GetString()! : "").ToArray();
        if (ids.Length is < 1 or > 8 || ids.Distinct(StringComparer.Ordinal).Count() != ids.Length
            || ids.Any(id => id.Length is < 1 or > 16 || id.Any(c => !char.IsAsciiLetterLower(c) && !char.IsAsciiDigit(c)))) throw new ModException("invalid games");
        return ids;
    }

    public static string Hash(ReadOnlySpan<byte> bytes) => Convert.ToHexStringLower(SHA256.HashData(bytes));

    public static void ValidatePath(string path)
    {
        if (string.IsNullOrWhiteSpace(path) || path.Contains('\\') || Path.IsPathRooted(path) || path.Contains(':')
            || path.Any(c => c < 32 || "<>\"|?*".Contains(c, StringComparison.Ordinal))) throw new ModException("invalid mod path");
        foreach (var part in path.Split('/'))
        {
            var stem = part.Split('.')[0];
            var tilde = part.IndexOf('~', StringComparison.Ordinal);
            // An 8.3 short-name alias (DATA~1, AZIMUT~1.EXE) could name a protected path under another spelling.
            if (tilde >= 0 && tilde + 1 < part.Length && char.IsAsciiDigit(part[tilde + 1])) throw new ModException("invalid mod path");
            if (part is "" or "." or ".." || part.EndsWith('.') || part.EndsWith(' ')
                || new[] { "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9" }.Contains(stem, StringComparer.OrdinalIgnoreCase))
                throw new ModException("invalid mod path");
        }
    }

    internal static void RejectDuplicates(JsonElement element)
    {
        if (element.ValueKind == JsonValueKind.Object)
        {
            var names = new HashSet<string>(StringComparer.Ordinal);
            foreach (var property in element.EnumerateObject())
            {
                if (!names.Add(property.Name)) throw new ModException("duplicate JSON property");
                RejectDuplicates(property.Value);
            }
        }
        else if (element.ValueKind == JsonValueKind.Array)
            foreach (var item in element.EnumerateArray()) RejectDuplicates(item);
    }
}

/// <summary>One mapped file: its root (<c>azimuth</c>; <c>game</c> only in a retired installation, OD-232), source, destination and pin.</summary>
public sealed record ManifestFile(string Root, string Source, string Destination, string Hash, long Bytes, string Role, string Mode, string Package = "primary");
public sealed record ManifestDependency(string Id, IReadOnlyList<string> Versions);
public sealed record ModWineOverride(string Module, bool AppDefaults);
/// <summary>OD-117: one of the player's own sources a mod reads (a folder, a file such as a ZIP, or either), with its texts.</summary>
public sealed record ExternalSource(string Id, string Kind, JsonElement Label, JsonElement Help, string? Hint)
{
    public string LabelText(string locale) => Localized(Label, locale);
    public string HelpText(string locale) => Localized(Help, locale);
    private static string Localized(JsonElement text, string locale) => text.TryGetProperty(locale, out var value) ? value.GetString()! : text.GetProperty("en").GetString()!;
}
