// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text.Json;

namespace Infinity.Core.Mods;

/// <summary>A <c>config.openFolders</c> entry: an Azimuth-relative folder outside <c>mods\</c> (forward slashes), its localized button label, and whether opening creates it.</summary>
public sealed record ModOpenFolder(string Path, JsonElement Label, bool Create)
{
    /// <summary>The label in <paramref name="locale"/>, else English.</summary>
    public string Text(string locale) => (Label.TryGetProperty(locale, out var text) ? text : Label.GetProperty("en")).GetString()!;

    /// <summary>The folder's absolute path under <paramref name="azimuthRoot"/>.</summary>
    public string Under(string azimuthRoot) => System.IO.Path.Combine(azimuthRoot, Path.Replace('/', System.IO.Path.DirectorySeparatorChar));
}

/// <summary>
/// A <c>folder</c> option of a mod's settings (EasyRadio, CR batch 1118). Its INI value is an absolute local folder (a drive path
/// <c>X:\…</c>, a UNC path <c>\\server\share\…</c>, or under Proton the Wine path the picker returns, <c>Z:\home\…</c>), the option's
/// <c>emptyValue</c> (no folder), or, with <c>tokenFolder</c>, a plain folder name read as <c>&lt;Azimuth&gt;\&lt;tokenFolder&gt;\&lt;name&gt;</c>
/// (the older enum <c>tokens</c> such as <c>music1</c> among them). Tokens and the empty value compare case-insensitively, as the
/// plugin reads them. A folder that is not there now is valid (a USB drive unplugged): the form shows a hint, never a settings issue.
/// What the launcher writes is strict (<see cref="Valid"/>); what it reads is also every value the plugin itself uses
/// (<see cref="PluginUses"/>), so Repair never resets a folder the mod plays from.
/// </summary>
public static class ModFolderOption
{
    public const string Type = "folder";
    public const int MaxLength = 1024;

    private static readonly string[] Reserved = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9"];

    /// <summary>Folder names the plugin refuses as a plain name (its own generated folders).</summary>
    private static readonly string[] PluginOwnNames = ["plugins", "media", "cache"];

    /// <summary>The environment the Wine prefix is read from (tests replace it).</summary>
    internal static Func<string, string?> Environment { get; set; } = System.Environment.GetEnvironmentVariable;

    /// <summary>The option's no-folder token (<c>emptyValue</c>).</summary>
    public static string EmptyValue(JsonElement option) => option.GetProperty("emptyValue").GetString()!;

    /// <summary>The older enum tokens the option still accepts (<c>tokens</c>; none when it lists none).</summary>
    public static IReadOnlyList<string> Tokens(JsonElement option) => option.TryGetProperty("tokens", out var tokens)
        ? tokens.EnumerateArray().Select(t => t.GetString()!).ToArray() : [];

    /// <summary>The Azimuth-relative folder plain names live in (<c>tokenFolder</c>), or null.</summary>
    public static string? TokenFolder(JsonElement option) => option.TryGetProperty("tokenFolder", out var folder) ? folder.GetString() : null;

    /// <summary>Whether <paramref name="value"/> is the option's empty value (any case).</summary>
    public static bool IsEmpty(JsonElement option, string? value) => string.Equals(value, EmptyValue(option), StringComparison.OrdinalIgnoreCase);

    /// <summary>
    /// A plain folder name the option stores (with <c>tokenFolder</c> only): the token characters <c>[A-Za-z0-9_-]{1,64}</c>, not the
    /// empty value, not a name the plugin keeps for itself (<c>plugins</c>, <c>media</c>, <c>cache</c>) nor a reserved device name.
    /// </summary>
    public static bool IsName(JsonElement option, string? value) => TokenFolder(option) is not null && value is { Length: > 0 and <= 64 }
        && value.All(c => char.IsAsciiLetterOrDigit(c) || c is '_' or '-') && !IsEmpty(option, value)
        && !PluginOwnNames.Contains(value, StringComparer.OrdinalIgnoreCase) && !Reserved.Contains(value, StringComparer.OrdinalIgnoreCase);

    /// <summary>
    /// Whether <paramref name="value"/> is a folder this option may store: at most <see cref="MaxLength"/> characters, no control
    /// character, no <c>;</c> or <c>#</c> (the INI reader cuts the value at either), no <c>"</c> or other character a Windows path cannot
    /// hold, an absolute drive or UNC path whose segments follow the manifest's path rules (no empty, <c>.</c> or <c>..</c> segment, none
    /// ending in a dot or a space, no 8.3 alias <c>~digit</c>, no reserved device name), a UNC host that is not this PC (loopback,
    /// its own name) and no administrative share (<c>C$</c>, <c>ADMIN$</c>, <c>IPC$</c>), and not the mod's own folder
    /// <paramref name="modFolder"/> or inside it, Proton's <c>Z:\…\pfx\drive_c</c> read as <c>C:</c> (null: not checked). This is the
    /// text check; <see cref="InsideOnDisk"/> resolves links and short names of a folder that is there.
    /// </summary>
    public static bool ValidPath(string? value, string? modFolder)
    {
        if (value is null || value.Length is < 3 or > MaxLength || value.Trim().Length != value.Length) return false;
        if (value.Any(c => char.IsControl(c) || c is ';' or '#' or '"' or '<' or '>' or '|' or '?' or '*')) return false;
        var path = value.Replace('/', '\\');
        string[] segments;
        if (path.StartsWith(@"\\", StringComparison.Ordinal))
        {
            // UNC: \\server\share[\…]; never a device or long-path prefix (\\?\, \\.\), this PC itself or an administrative share
            segments = path[2..].Split('\\');
            if (segments.Length < 2 || segments[0].Length == 0 || segments[1].Length == 0 || segments[0] is "?" or "." || ThisPc(segments[0])
                || segments[1].Length == 2 && char.IsAsciiLetter(segments[1][0]) && segments[1][1] == '$'
                || segments[1].Equals("ADMIN$", StringComparison.OrdinalIgnoreCase) || segments[1].Equals("IPC$", StringComparison.OrdinalIgnoreCase)) return false;
        }
        else if (char.IsAsciiLetter(path[0]) && path[1] == ':' && path[2] == '\\') segments = path[3..].Split('\\');
        else return false;
        for (var i = 0; i < segments.Length; i++)
        {
            var segment = segments[i];
            if (segment.Length == 0) { if (i == segments.Length - 1 && i > (path.StartsWith(@"\\", StringComparison.Ordinal) ? 1 : -1)) continue; return false; }
            if (segment.Contains(':', StringComparison.Ordinal) || segment is "." or ".." || segment.EndsWith('.') || segment.EndsWith(' ')) return false;
            var tilde = segment.IndexOf('~', StringComparison.Ordinal);
            if (tilde >= 0 && tilde + 1 < segment.Length && char.IsAsciiDigit(segment[tilde + 1])) return false;
            if (Reserved.Contains(segment.Split('.')[0].TrimEnd(), StringComparer.OrdinalIgnoreCase)) return false;
        }
        return modFolder is null || !Inside(path, modFolder);
    }

    /// <summary>
    /// For a folder that is there: whether its real path (every link and junction followed, short names expanded) is the mod's own
    /// folder or inside it, compared with the mod folder's real path. A folder that is not there: false; one that cannot be resolved:
    /// true (refused). Disk work: call it off the UI thread.
    /// </summary>
    public static bool InsideOnDisk(string value, string? modFolder)
    {
        ArgumentNullException.ThrowIfNull(value);
        if (modFolder is null) return false;
        try
        {
            if (!Directory.Exists(value)) return false;
            return Inside(Real(value, 0), Real(modFolder, 0));
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException or System.Security.SecurityException)
        {
            return true;
        }
    }

    /// <summary>Whether <paramref name="value"/> is valid for the option when the launcher writes it: its empty value, a token or plain name, or a <see cref="ValidPath"/>.</summary>
    public static bool Valid(JsonElement option, string? value, string? modFolder) =>
        value is not null && (IsEmpty(option, value) || Tokens(option).Contains(value, StringComparer.OrdinalIgnoreCase) || IsName(option, value) || ValidPath(value, modFolder));

    /// <summary>
    /// Whether the plugin uses <paramref name="raw"/> (EasyRadio's <c>er_ini.c</c> <c>good_folder</c>): the empty value, an absolute
    /// path (<c>X:\</c>, <c>X:/</c>, <c>\\</c>, <c>/</c>) or a plain name, never ending in a dot or a space, no control character
    /// nor <c>* ? " &lt; &gt; |</c>, a <c>:</c> only as a drive's, a plain name not starting with a dot nor one of its own folders.
    /// </summary>
    public static bool PluginUses(JsonElement option, string? raw)
    {
        if (raw is null) return false;
        if (IsEmpty(option, raw)) return true;
        var n = raw.Length;
        if (n == 0 || raw[^1] is '.' or ' ') return false;
        var absolute = n >= 3 && char.IsAsciiLetter(raw[0]) && raw[1] == ':' && raw[2] is '\\' or '/' || n >= 3 && raw[0] == '\\' && raw[1] == '\\' || n >= 2 && raw[0] == '/';
        if (!absolute && raw[0] == '.') return false;
        for (var i = 0; i < n; i++)
        {
            var c = raw[i];
            var separator = c is '/' or '\\' || c == ':' && i == 1;
            if (c < 32 || separator && !absolute || c == ':' && i != 1 || c is '*' or '?' or '"' or '<' or '>' or '|') return false;
        }
        return absolute || !PluginOwnNames.Contains(raw, StringComparer.OrdinalIgnoreCase);
    }

    /// <summary>
    /// The absolute folder a value names: an absolute path itself; a plain name (a token included) under
    /// <c>&lt;Azimuth&gt;\&lt;tokenFolder&gt;</c> (<paramref name="azimuthRoot"/> null: none); none for the empty value.
    /// </summary>
    public static string? Resolve(JsonElement option, string value, string? azimuthRoot)
    {
        ArgumentNullException.ThrowIfNull(value);
        if (IsEmpty(option, value)) return null;
        var absolute = value.Length >= 3 && char.IsAsciiLetter(value[0]) && value[1] == ':' && value[2] is '\\' or '/' || value.StartsWith(@"\\", StringComparison.Ordinal) || value.StartsWith('/');
        if (!absolute && TokenFolder(option) is { } folder)
            return azimuthRoot is null ? null : Path.Combine(azimuthRoot, folder.Replace('/', Path.DirectorySeparatorChar), value);
        return value;
    }

    /// <summary>Whether <paramref name="optionId"/> names a <c>folder</c> option of <paramref name="manifest"/> (never a required or multiplayer setting).</summary>
    public static bool IsFolderSetting(ModManifest manifest, string? optionId) =>
        (manifest ?? throw new ArgumentNullException(nameof(manifest))).Document.TryGetProperty("config", out var config) && config.ValueKind == JsonValueKind.Object
        && config.TryGetProperty("options", out var options)
        && options.EnumerateArray().Any(o => o.GetProperty("id").GetString() == optionId && o.GetProperty("type").GetString() == Type);

    /// <summary>The absolute folder of a mod installed under <paramref name="azimuthRoot"/>, or null when its files name no single folder.</summary>
    public static string? ModFolder(ModManifest manifest, string? azimuthRoot) =>
        azimuthRoot is not null && ExtraOptionsMods.FolderOf(manifest ?? throw new ArgumentNullException(nameof(manifest))) is { } folder
            ? Path.Combine(azimuthRoot, ExtraOptionsMods.FolderPrefix(manifest.ModsGame, folder).TrimEnd('/').Replace('/', '\\')) : null;

    private static bool ThisPc(string host)
    {
        if (host.Equals("localhost", StringComparison.OrdinalIgnoreCase) || host.EndsWith(".localhost", StringComparison.OrdinalIgnoreCase)
            || host.EndsWith(".ipv6-literal.net", StringComparison.OrdinalIgnoreCase) || host.Equals(System.Environment.MachineName, StringComparison.OrdinalIgnoreCase))
            return true;
        return System.Net.IPAddress.TryParse(host, out var address)
            && (System.Net.IPAddress.IsLoopback(address) || address.Equals(System.Net.IPAddress.Any) || address.Equals(System.Net.IPAddress.IPv6Any));
    }

    private static bool Inside(string path, string folder)
    {
        var a = Normal(path); var b = Normal(folder);
        return a.Equals(b, StringComparison.OrdinalIgnoreCase) || a.StartsWith(b + "\\", StringComparison.OrdinalIgnoreCase);
    }

    /// <summary>Separators collapsed, <c>.</c> segments dropped, and Proton's <c>Z:\&lt;prefix&gt;\drive_c</c> read as <c>C:</c>.</summary>
    private static string Normal(string path)
    {
        path = path.Replace('/', '\\');
        foreach (var driveC in WineDriveC())
            if (path.StartsWith(driveC, StringComparison.OrdinalIgnoreCase) && (path.Length == driveC.Length || path[driveC.Length] == '\\'))
            {
                path = "C:" + (path.Length == driveC.Length ? "\\" : path[driveC.Length..]);
                break;
            }
        var unc = path.StartsWith(@"\\", StringComparison.Ordinal);
        var parts = path.Split('\\', StringSplitOptions.RemoveEmptyEntries).Where(p => p != ".");
        return (unc ? @"\\" : "") + string.Join('\\', parts);
    }

    /// <summary>The Wine prefix's <c>drive_c</c> as Wine shows it under <c>Z:</c> (<c>WINEPREFIX</c>, Proton's <c>STEAM_COMPAT_DATA_PATH\pfx</c>).</summary>
    private static IEnumerable<string> WineDriveC()
    {
        static string? Unix(string? path) => path is { Length: > 1 } && path[0] == '/' ? "Z:" + path.TrimEnd('/').Replace('/', '\\') : null;
        if (Unix(Environment("WINEPREFIX")) is { } prefix) yield return prefix + @"\drive_c";
        if (Unix(Environment("STEAM_COMPAT_DATA_PATH")) is { } data) yield return data + @"\pfx\drive_c";
    }

    /// <summary>The real path of a folder: each existing segment's own name (a short 8.3 name expanded) and every link followed.</summary>
    private static string Real(string path, int depth)
    {
        if (depth > 40) throw new IOException("too many links");
        var full = Path.TrimEndingDirectorySeparator(Path.GetFullPath(path));
        if (Path.GetDirectoryName(full) is not { } parent) return full;
        var name = Path.GetFileName(full);
        var realParent = Real(parent, depth + 1);
        if (Directory.Exists(Path.Combine(realParent, name)) && name.IndexOfAny(['*', '?']) < 0
            && new DirectoryInfo(realParent).EnumerateDirectories(name).FirstOrDefault() is { } entry) name = entry.Name;
        var current = Path.Combine(realParent, name);
        return Directory.Exists(current) && Directory.ResolveLinkTarget(current, returnFinalTarget: true) is { } target ? Real(target.FullName, depth + 1) : current;
    }
}