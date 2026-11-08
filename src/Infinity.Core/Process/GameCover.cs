// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Process;

/// <summary>
/// Owner 2026-11-08 (games-5): covers from the user's own game files. A game tile's cover is read at runtime, read only, from this PC:
/// <c>data\azimuth.ini</c> → the title's game folder (<see cref="AzimuthTitles.GameFolder"/>) → <see cref="GameDefinition.CoverPath"/>
/// (<c>Mount\Logo.png</c>). Nothing is shipped or written (rule 3). A missing or unreadable file, or one that is not a JPEG or PNG of at most
/// <see cref="MaxBytes"/>, is no cover: the launcher logs <see cref="CoverRead.Problem"/> and draws the styled tile.
/// </summary>
public static class GameCover
{
    public const long MaxBytes = 16 * 1024 * 1024;

    /// <summary>The cover of <paramref name="game"/> in the Azimuth install at <paramref name="azimuthRoot"/>: the bytes, or why there are none.</summary>
    public static CoverRead Read(string? azimuthRoot, GameDefinition game)
    {
        ArgumentNullException.ThrowIfNull(game);
        if (string.IsNullOrWhiteSpace(azimuthRoot)) return new(null, null, "no Azimuth folder");
        var folder = AzimuthTitles.GameFolder(azimuthRoot, game);
        if (string.IsNullOrWhiteSpace(folder)) return new(null, null, "no game folder in azimuth.ini");
        if (Path.IsPathRooted(game.CoverPath) || game.CoverPath.Contains("..", StringComparison.Ordinal)) return new(null, null, "cover path not relative");
        var path = Path.Combine(folder, game.CoverPath);
        try
        {
            var file = new FileInfo(path);
            if (!file.Exists) return new(null, path, "missing");
            if (file.Length is <= 0 or > MaxBytes) return new(null, path, "size out of range");
            var bytes = File.ReadAllBytes(path);
            return IsImage(bytes) ? new(bytes, path, null) : new(null, path, "not a JPEG or PNG");
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException)
        {
            return new(null, path, "unreadable (" + error.GetType().Name + ")");
        }
    }

    /// <summary>A JPEG (<c>FF D8 FF</c>) or a PNG (<c>89 50 4E 47 0D 0A 1A 0A</c>) by its first bytes.</summary>
    public static bool IsImage(ReadOnlySpan<byte> bytes) =>
        bytes is [0xFF, 0xD8, 0xFF, ..] || bytes is [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, ..];
}

/// <summary>A cover read: the bytes and the file, or the problem the launcher logs before it draws the styled tile.</summary>
public sealed record CoverRead(byte[]? Bytes, string? Source, string? Problem);
