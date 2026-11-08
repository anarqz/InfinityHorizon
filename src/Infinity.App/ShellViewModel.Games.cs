// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using System.Security.Cryptography;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-08: a game tile of PLAY's row: the game's cover (OD-217: shipped in the package, <see cref="GameCover"/>), else the styled
/// tile (the catalog's short code, tagline and accent); the name small under it, and why a tile is dimmed.
/// </summary>
public sealed record GameCard(GameInfo Info, string Title, string ShortCode, string Tagline, Brush Accent, Brush Soft, bool Selected)
{
    public ImageSource? Cover { get; init; }
    public bool HasCover => Cover is not null;
    public bool Enabled { get; init; } = true;
    public string Reason { get; init; } = string.Empty;
    public bool HasReason => Reason.Length > 0;
}

/// <summary>
/// OD-206: which games this PC offers. The allowed titles (<see cref="GameDefinition.All"/>) found in Azimuth's <c>data\titles</c>;
/// signed in, also those the server lists (a game it does not list stays hidden). Each game's folder comes from <c>data\azimuth.ini</c>.
/// PLAY's tiles show every catalog game and dim the ones this install lacks (<see cref="PlayTiles"/>).
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

    /// <summary>
    /// PLAY's tiles (games-5): every catalog game (<see cref="PlayTiles"/>), those this install lacks dimmed with the reason; no request. The
    /// covers load off the UI thread once and are cached; a missing or unreadable one is logged and the tile stays the styled one.
    /// </summary>
    private void FillGameCards()
    {
        GameCards.Clear();
        var served = OfflineMode || _session is null ? null : _session.Info.Games ?? [];
        foreach (var tile in PlayTiles.Build(Games, served))
        {
            var definition = tile.Game;
            var game = tile.Info;
            var art = ArtOf(definition);
            GameCards.Add(new GameCard(game, art?.Name ?? definition.Name, definition.ShortName, definition.TileTagline,
                TileBrush(definition.TileAccent, 0xFF), TileBrush(definition.TileAccent, 0x2E), game.Id == _gate.Game)
            {
                Cover = art?.Image,
                Enabled = tile.Selectable,
                Reason = tile.State switch
                {
                    PlayTileState.NotInInstall => S["play.tile_not_in_install"],
                    PlayTileState.Closed => S["play.tile_closed"],
                    _ => string.Empty,
                },
            });
        }
    }

    /// <summary>A tile's art from this PC (owner 2026-11-08: covers from the user's own game files): the cover (null: the styled tile) and the ini's name.</summary>
    private sealed record TileArt(ImageSource? Image, string? Name);

    /// <summary>The art per Azimuth folder and game, read and decoded once off the UI thread, at tile size.</summary>
    private readonly Dictionary<string, TileArt> _tileArt = new(StringComparer.OrdinalIgnoreCase);
    private readonly HashSet<string> _tileArtLoading = new(StringComparer.OrdinalIgnoreCase);

    /// <summary>The cover's decoded width: the 168 px square tile, twice for high DPI.</summary>
    private const int CoverDecodePixels = 336;

    /// <summary>The cached art, or null while it loads (the load fills the tiles again when done).</summary>
    private TileArt? ArtOf(GameDefinition game)
    {
        var root = _install?.Root;
        if (string.IsNullOrWhiteSpace(root)) return null;
        var key = root + "|" + game.Id;
        if (_tileArt.TryGetValue(key, out var cached)) return cached;
        if (_tileArtLoading.Add(key)) _ = LoadTileArtAsync(key, root, game);
        return null;
    }

    private async Task LoadTileArtAsync(string key, string root, GameDefinition game)
    {
        var (art, problem) = await Task.Run(() =>
        {
            var read = GameCover.Read(root, game); // read only: nothing in Azimuth or the game folder is ever written
            var (image, decodeProblem) = DecodeCover(read);
            return (new TileArt(image, AzimuthTitles.DisplayName(root, game)), read.Problem ?? decodeProblem);
        }).ConfigureAwait(true);
        _tileArt[key] = art;
        _tileArtLoading.Remove(key);
        Log(art.Image is null ? $"play: {game.Id} cover {problem}; the styled tile is shown" : $"play: {game.Id} cover read from the game folder");
        if (_install?.Root == root) FillGameCards();
    }

    private static (ImageSource? Image, string? Problem) DecodeCover(CoverRead read)
    {
        if (read.Bytes is not { } bytes) return (null, null);
        try
        {
            var bitmap = new BitmapImage();
            using (var stream = new MemoryStream(bytes))
            {
                bitmap.BeginInit();
                bitmap.CacheOption = BitmapCacheOption.OnLoad;
                bitmap.DecodePixelWidth = CoverDecodePixels;
                bitmap.StreamSource = stream;
                bitmap.EndInit();
            }
            bitmap.Freeze();
            return (bitmap, null);
        }
        catch (Exception error) when (error is IOException or NotSupportedException or InvalidOperationException or ArgumentException or FileFormatException)
        {
            return (null, "not decoded (" + error.GetType().Name + ")");
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
