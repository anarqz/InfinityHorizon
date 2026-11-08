// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using System.Security.Cryptography;
using System.Windows.Media;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-08: a game tile of PLAY's row. Our own art only (rule 3: no game file or game art): the catalog's short code, name, tagline
/// and accent (<see cref="GameDefinition.TileAccent"/>).
/// </summary>
public sealed record GameCard(GameInfo Info, string Title, string ShortCode, string Tagline, Brush Accent, Brush Soft, bool Selected)
{
    public bool Enabled => Info.Accepting;
}

/// <summary>
/// OD-206: which games this PC offers. The allowed titles (<see cref="GameDefinition.All"/>) found in Azimuth's <c>data\titles</c>;
/// signed in, also those the server lists (a closed game stays hidden). Each game's folder and cover come from <c>data\azimuth.ini</c>.
/// </summary>
public sealed partial class ShellViewModel
{
    public ObservableCollection<GameCard> GameCards { get; } = [];

    /// <summary>The chosen game; FH2 until one is.</summary>
    private GameDefinition CurrentGame => GameDefinition.ById(_gate.Game) ?? GameDefinition.Fh2;

    /// <summary>The offline list: every available game, as the server's list would describe it.</summary>
    private void FillOfflineGames()
    {
        Games.Clear();
        foreach (var game in GameDefinition.Available(_install?.Root))
        {
            Games.Add(new GameInfo(game.Id, game.Name, "", "", true, null));
        }
    }

    /// <summary>The signed-in list: what the server serves, of what this PC has.</summary>
    private void FillServerGames(IEnumerable<GameInfo> served)
    {
        var available = GameDefinition.Available(_install?.Root);
        Games.Clear();
        foreach (var game in served)
        {
            if (GameDefinition.ById(game.Id) is { } known && available.Contains(known)) Games.Add(game);
        }
    }

    /// <summary>PLAY's tiles follow <see cref="Games"/> and the selected game (no file or request: the catalog's data only).</summary>
    private void FillGameCards()
    {
        GameCards.Clear();
        foreach (var game in Games)
        {
            var definition = GameDefinition.ById(game.Id);
            var accent = TileBrush(definition?.TileAccent, 0xFF);
            GameCards.Add(new GameCard(game, game.Title, definition?.ShortName ?? game.Id.ToUpperInvariant(), definition?.TileTagline ?? string.Empty,
                accent, TileBrush(definition?.TileAccent, 0x2E), game.Id == _gate.Game));
        }
    }

    private static SolidColorBrush TileBrush(string? argb, byte alpha)
    {
        Color color;
        try { color = argb is null ? Color.FromRgb(0xB0, 0x6B, 0xFF) : (Color)ColorConverter.ConvertFromString(argb); }
        catch (FormatException) { color = Color.FromRgb(0xB0, 0x6B, 0xFF); }
        var brush = new SolidColorBrush(Color.FromArgb(alpha, color.R, color.G, color.B));
        brush.Freeze();
        return brush;
    }

    private PlaySelection? _playSelection;

    /// <summary>
    /// Owner 2026-11-08: the game PLAY opens on after sign-in or Play offline: the last one selected (local settings) while it is still
    /// offered, else the first. Null when no game is offered.
    /// </summary>
    private string? OpenPlayGame()
    {
        _playSelection ??= new PlaySelection(_stateDirectory);
        try { return _playSelection.Open(Games.Select(g => g.Id)); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            Log("games: last game not read (" + error.GetType().Name + ")");
            return Games.FirstOrDefault()?.Id;
        }
    }

    private void RememberGame(string id)
    {
        _playSelection ??= new PlaySelection(_stateDirectory);
        try { _playSelection.Select(id, Games.Select(g => g.Id)); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or ArgumentException) { Log("games: last game not saved (" + error.GetType().Name + ")"); }
    }

    /// <summary>The folder a game other than FH2 plays from when none was chosen in Configure: the one <c>data\azimuth.ini</c> names.</summary>
    private Infinity.Core.Integrity.GameContentLocation? IniContentLocation(GameDefinition game) =>
        game.FullFeatures ? null : Infinity.Core.Integrity.GameContentLocation.Detect(AzimuthTitles.GameFolder(_install?.Root, game), game);

    /// <summary>What the gate compares with the hook's X-Fh2-CarDb when no integrity check declares it: empty if it cannot be read.</summary>
    private async Task<string> CarDatabaseDigestAsync(GameDefinition game)
    {
        if (ContentLocation is not { } location) return string.Empty;
        try
        {
            await using var input = File.OpenRead(Path.Combine(location.Mount, game.CarDatabase));
            return Convert.ToHexStringLower(await SHA256.HashDataAsync(input));
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            Log("play: car database not read (" + error.GetType().Name + ")");
            return string.Empty;
        }
    }
}
