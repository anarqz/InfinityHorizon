// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Mods;

/// <summary>What the launcher does with one file of a release (OD-45, docs/launcher/MODS.md).</summary>
public enum ModFileRole
{
    /// <summary>The mod's code (e.g. <c>winmm.dll</c>): replaced on update, MD5-checked, moved aside when the mod is off.</summary>
    Binary,

    /// <summary>The player's settings (the INI): installed only when absent, never overwritten.</summary>
    Config,

    /// <summary>A file the player may replace (the Gamerpic): installed only when absent.</summary>
    UserAsset,
}

/// <summary>One file of a release: where it is in the official zip, where it goes in the Azimuth folder, its MD5.</summary>
public sealed record ModFile(string ZipPath, string Destination, string Md5, ModFileRole Role);

/// <summary>A release the project vetted: the official asset (name and SHA-256 as GitHub serves it) and its files.</summary>
public sealed record ModRelease(string Version, string AzimuthVersion, string Tag, string AssetName, string AssetSha256, IReadOnlyList<ModFile> Files, IReadOnlyList<string> NeverInstalled)
{
    public IEnumerable<ModFile> Binaries => Files.Where(f => f.Role == ModFileRole.Binary);

    /// <summary>The asset's download URL on the official repository (GitHub's release-download path; the SHA-256 is what is trusted).</summary>
    public Uri DownloadUrl(ModInfo mod) => new($"https://github.com/{mod?.GitHubRepo}/releases/download/{Uri.EscapeDataString(Tag)}/{Uri.EscapeDataString(AssetName)}");
}

/// <summary>A vetted mod: credits, official repository, and the releases the launcher may install.</summary>
public sealed record ModInfo(string Id, string Name, string Author, string License, string Repository, string GitHubRepo, IReadOnlyList<ModRelease> Releases)
{
    /// <summary>The newest vetted release.</summary>
    public ModRelease Latest => Releases.OrderByDescending(r => ModVersion.Parse(r.Version)).First();

    /// <summary>The newest vetted release built for <paramref name="azimuthVersion"/>; null when none (or the version is unknown).</summary>
    public ModRelease? For(string? azimuthVersion) => azimuthVersion is null ? null
        : Releases.Where(r => r.AzimuthVersion == azimuthVersion).OrderByDescending(r => ModVersion.Parse(r.Version)).FirstOrDefault();

    public ModRelease? Release(string version) => Releases.FirstOrDefault(r => r.Version == version);
}

/// <summary>
/// The mods the project supports (OD-45): only these are installed, and only the releases listed
/// here, verified by the asset's SHA-256 and each file's MD5. A newer release on GitHub shows as
/// "not verified yet" until it is added here.
/// </summary>
public static class ModCatalog
{
    public const string ExtraOptionsId = "extraoptions";

    /// <summary>
    /// ExtraOptions by Arthur (RokkuDayo), GPL-3.0. Releases checked on 2026-10-19 (asset SHA-256
    /// equal to GitHub's digest). Only 0.2.98 is a <c>winmm.dll</c> next to the hook; 0.2.34-0.2.74
    /// are themselves an <c>azimuth-tls.dll</c>, which would replace the hook, so they are not here.
    /// The zip's <c>data\bin\azimuth-tls.dll</c> is Azimuth's stock helper: never installed.
    /// </summary>
    public static ModInfo ExtraOptions { get; } = new(
        ExtraOptionsId,
        "Extra Options",
        "Arthur (RokkuDayo)",
        "GPL-3.0",
        "https://github.com/RokkuDayo/ForzaHorizon2_ExtraOptions",
        "RokkuDayo/ForzaHorizon2_ExtraOptions",
        [
            new ModRelease(
                "0.2.98",
                "0.9.10",
                "Azimuth_v0.9.10",
                "FH2_Extra_Options_0.2.98-release.zip",
                "d3b9db715416abd8efef4048445a9edbe22d333ba0267fb48fb255dfab000c38",
                [
                    new ModFile("Azimuth/winmm.dll", "winmm.dll", "76e96191f84a1a5baceeea99c7b544fe", ModFileRole.Binary),
                    new ModFile("Azimuth/FH2_ExtraOptions.ini", "FH2_ExtraOptions.ini", "1a7901eb51a95434399efd55386046be", ModFileRole.Config),
                    new ModFile("Azimuth/Gamerpic.png", "Gamerpic.png", "7542c2d40b22d62d05c5ece3707e8ca2", ModFileRole.UserAsset),
                ],
                ["Azimuth/data/bin/azimuth-tls.dll"]),
        ]);

    public static IReadOnlyList<ModInfo> All { get; } = [ExtraOptions];
}

/// <summary>A dotted numeric version ("0.2.98"), compared number by number.</summary>
public sealed record ModVersion(IReadOnlyList<int> Parts) : IComparable<ModVersion>
{
    public static ModVersion Parse(string text) => TryParse(text) ?? throw new FormatException($"not a version: {text}");

    /// <summary>The first dotted number in <paramref name="text"/> ("v0.2.98-latest", "FH2_Extra_Options_0.2.98-release.zip").</summary>
    public static ModVersion? TryParse(string? text)
    {
        if (text is null)
        {
            return null;
        }
        var match = System.Text.RegularExpressions.Regex.Match(text, @"\d+(\.\d+)+");
        if (!match.Success)
        {
            return null;
        }
        var parts = new List<int>();
        foreach (var part in match.Value.Split('.'))
        {
            if (!int.TryParse(part, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var n))
            {
                return null;
            }
            parts.Add(n);
        }
        return new ModVersion(parts);
    }

    public int CompareTo(ModVersion? other)
    {
        if (other is null)
        {
            return 1;
        }
        for (var i = 0; i < Math.Max(Parts.Count, other.Parts.Count); i++)
        {
            var a = i < Parts.Count ? Parts[i] : 0;
            var b = i < other.Parts.Count ? other.Parts[i] : 0;
            if (a != b)
            {
                return a.CompareTo(b);
            }
        }
        return 0;
    }

    public override string ToString() => string.Join('.', Parts);

    public static bool operator <(ModVersion left, ModVersion right) => left is null ? right is not null : left.CompareTo(right) < 0;

    public static bool operator >(ModVersion left, ModVersion right) => left is not null && left.CompareTo(right) > 0;

    public static bool operator <=(ModVersion left, ModVersion right) => !(left > right);

    public static bool operator >=(ModVersion left, ModVersion right) => !(left < right);
}
