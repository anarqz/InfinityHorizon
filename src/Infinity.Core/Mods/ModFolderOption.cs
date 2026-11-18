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
/// A <c>folder</c> option of a mod's settings (EasyRadio, CR batch 1118): its INI value is an absolute local folder (a drive path
/// <c>X:\…</c>, a UNC path <c>\\server\share\…</c>, or under Proton the Wine path the picker returns, <c>Z:\home\…</c>), the option's
/// <c>emptyValue</c> token (no folder), or one of its older <c>tokens</c> (an enum value an earlier version wrote, read as the folder of
/// that name under <c>&lt;Azimuth&gt;\&lt;tokenFolder&gt;</c>). A folder that is not there now is a valid value (a USB drive unplugged):
/// the form shows a hint, never a settings issue.
/// </summary>
public static class ModFolderOption
{
    public const string Type = "folder";
    public const int MaxLength = 1024;

    /// <summary>The option's no-folder token (<c>emptyValue</c>).</summary>
    public static string EmptyValue(JsonElement option) => option.GetProperty("emptyValue").GetString()!;

    /// <summary>The older enum tokens the option still accepts (<c>tokens</c>; none when it lists none).</summary>
    public static IReadOnlyList<string> Tokens(JsonElement option) => option.TryGetProperty("tokens", out var tokens)
        ? tokens.EnumerateArray().Select(t => t.GetString()!).ToArray() : [];

    /// <summary>
    /// Whether <paramref name="value"/> is a folder this option may store: at most <see cref="MaxLength"/> characters, no control
    /// character, no <c>;</c> or <c>#</c> (the INI reader cuts the value at either, quoted or not), no <c>"</c> or other character a
    /// Windows path cannot hold, an absolute drive or UNC path without <c>.</c>/<c>..</c> segments, and not the mod's own folder
    /// <paramref name="modFolder"/> or inside it (an uninstall deletes that folder). <paramref name="modFolder"/> null: not checked.
    /// </summary>
    public static bool ValidPath(string? value, string? modFolder)
    {
        if (value is null || value.Length is < 3 or > MaxLength || value.Trim().Length != value.Length) return false;
        if (value.Any(c => char.IsControl(c) || c is ';' or '#' or '"' or '<' or '>' or '|' or '?' or '*')) return false;
        var path = value.Replace('/', '\\');
        string rest;
        if (path.StartsWith(@"\\", StringComparison.Ordinal))
        {
            // UNC: \\server\share[\…]; never a device or long-path prefix (\\?\, \\.\)
            var parts = path[2..].Split('\\');
            if (parts.Length < 2 || parts[0].Length == 0 || parts[1].Length == 0 || parts[0] is "?" or "." || parts[0].Contains(':', StringComparison.Ordinal)) return false;
            rest = path[2..];
        }
        else if (char.IsAsciiLetter(path[0]) && path[1] == ':' && path[2] == '\\') rest = path[3..];
        else return false;
        if (rest.Contains(':', StringComparison.Ordinal)) return false;
        var segments = rest.Split('\\');
        for (var i = 0; i < segments.Length; i++)
        {
            var segment = segments[i];
            if (segment is "." or "..") return false;
            // only one trailing separator (X:\ itself, or D:\Music\)
            if (segment.Length == 0 && !(i == segments.Length - 1)) return false;
        }
        return modFolder is null || !Inside(path, modFolder);
    }

    /// <summary>Whether <paramref name="value"/> is valid for the option: its empty token, one of its older tokens, or a <see cref="ValidPath"/>.</summary>
    public static bool Valid(JsonElement option, string? value, string? modFolder) =>
        value is not null && (value == EmptyValue(option) || Tokens(option).Contains(value, StringComparer.Ordinal) || ValidPath(value, modFolder));

    /// <summary>
    /// The absolute folder a value names: the path itself, an older token's folder under <c>&lt;Azimuth&gt;\&lt;tokenFolder&gt;</c>
    /// (<paramref name="azimuthRoot"/> null: none), or none for the empty token.
    /// </summary>
    public static string? Resolve(JsonElement option, string value, string? azimuthRoot)
    {
        ArgumentNullException.ThrowIfNull(value);
        if (value == EmptyValue(option)) return null;
        if (Tokens(option).Contains(value, StringComparer.Ordinal))
            return azimuthRoot is null ? null : Path.Combine(azimuthRoot, option.GetProperty("tokenFolder").GetString()!.Replace('/', Path.DirectorySeparatorChar), value);
        return value;
    }

    /// <summary>The absolute folder of a mod installed under <paramref name="azimuthRoot"/>, or null when its files name no single folder.</summary>
    public static string? ModFolder(ModManifest manifest, string? azimuthRoot) =>
        azimuthRoot is not null && ExtraOptionsMods.FolderOf(manifest ?? throw new ArgumentNullException(nameof(manifest))) is { } folder
            ? Path.Combine(azimuthRoot, ExtraOptionsMods.FolderPrefix(manifest.ModsGame, folder).TrimEnd('/').Replace('/', '\\')) : null;

    private static bool Inside(string path, string folder)
    {
        var a = Normal(path); var b = Normal(folder);
        return a.Equals(b, StringComparison.OrdinalIgnoreCase) || a.StartsWith(b + "\\", StringComparison.OrdinalIgnoreCase);
    }

    private static string Normal(string path)
    {
        var unc = path.Replace('/', '\\').StartsWith(@"\\", StringComparison.Ordinal);
        var parts = path.Replace('/', '\\').Split('\\', StringSplitOptions.RemoveEmptyEntries).Where(p => p != ".");
        return (unc ? @"\\" : "") + string.Join('\\', parts);
    }
}
