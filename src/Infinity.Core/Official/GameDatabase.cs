// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Official;

/// <summary>This PC's installed game database (gamedbRC.slt, read-only): the NR-6 parts rule judges the other players' cars against it.</summary>
public static class GameDatabase
{
    /// <summary>The game database under a Mount folder, as the game maps it (the first of mediaoverride, mediapc, media); null when none.</summary>
    public static string? Find(string mount) =>
        Process.CarNames.MediaRoots.Select(root => Path.Combine(mount, root, "stripped", "gamedbRC.slt")).FirstOrDefault(File.Exists);
}