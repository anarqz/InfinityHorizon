// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;
using Infinity.Core.Process;

namespace Infinity.Core.Shell;

/// <summary>What a game tile of PLAY can do (owner 2026-11-08, games-5).</summary>
public enum PlayTileState
{
    /// <summary>The game is in this Azimuth install and open: the tile selects it.</summary>
    Ready,

    /// <summary>The catalog has the game but this Azimuth install has no <c>data\titles\&lt;TitleId&gt;</c>: dimmed, "Not in this Azimuth install".</summary>
    NotInInstall,

    /// <summary>The server lists the game as not accepting players: dimmed (the old picker disabled it the same way).</summary>
    Closed,
}

/// <summary>One tile: the catalog game, the server's (or the offline) description, and its state.</summary>
public sealed record PlayTile(GameDefinition Game, GameInfo Info, PlayTileState State)
{
    public bool Selectable => State == PlayTileState.Ready;
}

/// <summary>
/// Owner 2026-11-08 (games-5): PLAY lists every catalog game (<see cref="GameDefinition.All"/>, in its order), not only those of the
/// selected Azimuth install. The old picker's rules stay: the games offered (<paramref name="offered"/>) are the catalog games found in
/// the install's <c>data\titles</c> (<see cref="GameDefinition.Available"/>: none found keeps FH2), and signed in only those the server
/// lists (a game the server does not list stays hidden; one it lists as not accepting is <see cref="PlayTileState.Closed"/>). A catalog game
/// the server lists (or any, offline) that the install lacks is shown as <see cref="PlayTileState.NotInInstall"/>, never hidden.
/// </summary>
public static class PlayTiles
{
    /// <param name="offered">The games the launcher can play now (its <c>Games</c> list: catalog ∩ install, ∩ server when signed in).</param>
    /// <param name="served">The server's game list; null offline (every catalog game counts as listed).</param>
    public static IReadOnlyList<PlayTile> Build(IEnumerable<GameInfo> offered, IEnumerable<GameInfo>? served)
    {
        ArgumentNullException.ThrowIfNull(offered);
        var here = offered.ToList();
        var listed = served?.ToList();
        var tiles = new List<PlayTile>();
        foreach (var game in GameDefinition.All)
        {
            if (here.FirstOrDefault(g => g.Id == game.Id) is { } info)
            {
                tiles.Add(new PlayTile(game, info, info.Accepting ? PlayTileState.Ready : PlayTileState.Closed));
                continue;
            }
            var server = listed?.FirstOrDefault(g => g.Id == game.Id);
            if (listed is not null && server is null) continue; // the server does not offer it: hidden, as before
            tiles.Add(new PlayTile(game, server ?? new GameInfo(game.Id, game.Name, "", "", true, null), PlayTileState.NotInInstall));
        }
        return tiles;
    }
}
