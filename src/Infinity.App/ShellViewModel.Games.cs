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

/// <summary>A game of the picker: its cover is null when <c>data\azimuth.ini</c> names no folder with one.</summary>
public sealed record GameCard(GameInfo Info, string Title, ImageSource? Cover);

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

    /// <summary>The cards follow <see cref="Games"/>; called from the constructor's subscription.</summary>
    private void FillGameCards()
    {
        GameCards.Clear();
        foreach (var game in Games)
        {
            var definition = GameDefinition.ById(game.Id);
            var folder = definition is null ? null : AzimuthTitles.GameFolder(_install?.Root, definition);
            GameCards.Add(new GameCard(game, game.Title, GameCoverImage(GameCover.Read(folder))));
        }
    }

    private static BitmapImage? GameCoverImage(byte[]? bytes)
    {
        if (bytes is null) return null;
        try
        {
            var bitmap = new BitmapImage();
            using (var stream = new MemoryStream(bytes)) { bitmap.BeginInit(); bitmap.CacheOption = BitmapCacheOption.OnLoad; bitmap.DecodePixelWidth = 480; bitmap.StreamSource = stream; bitmap.EndInit(); }
            bitmap.Freeze();
            return bitmap;
        }
        catch (Exception error) when (error is IOException or NotSupportedException or InvalidOperationException or ArgumentException or FileFormatException) { return null; }
    }

    /// <summary>The game to choose without asking: the only one, else the one last chosen while it is still offered.</summary>
    private string? GameToChooseAlone()
    {
        if (Games.Count == 1) return Games[0].Id;
        var last = LaunchModeStore.ReadLastGame(_stateDirectory);
        return Games.Any(game => game.Id == last) ? last : null;
    }

    private void RememberGame(string id)
    {
        try { LaunchModeStore.WriteLastGame(_stateDirectory, id); }
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
