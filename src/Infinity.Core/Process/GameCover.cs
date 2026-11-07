// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Process;

/// <summary>
/// A game's cover for the game picker, from the folder <c>data\azimuth.ini</c> names: the tile its <c>AppxManifest.xml</c>
/// declares for both games (<c>WideLogo.png</c>, 480x480, in spite of its name), else its logo. Fixed names and a size limit: the
/// picker never decodes anything else from a folder.
/// </summary>
public static class GameCover
{
    public const long MaxBytes = 16 * 1024 * 1024;
    private static readonly string[] Names = ["WideLogo.png", "Logo.png"];

    /// <summary>The cover's bytes; null when there is no readable one. <paramref name="gameFolder"/> holds <c>Mount</c>, or is it.</summary>
    public static byte[]? Read(string? gameFolder)
    {
        if (string.IsNullOrWhiteSpace(gameFolder)) return null;
        foreach (var mount in new[] { Path.Combine(gameFolder, "Mount"), gameFolder })
        {
            foreach (var name in Names)
            {
                try
                {
                    var file = new FileInfo(Path.Combine(mount, name));
                    if (!file.Exists) continue;
                    return file.Length is > 0 and <= MaxBytes ? File.ReadAllBytes(file.FullName) : null;
                }
                catch (Exception error) when (error is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException) { return null; }
            }
        }
        return null;
    }
}
