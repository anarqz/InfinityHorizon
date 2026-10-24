// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Globalization;
using System.IO;
using System.Net.Http;
using System.Runtime.CompilerServices;
using System.Text.Json;
using System.Windows;
using System.Windows.Documents;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using Infinity.Core.News;
using Infinity.Core.Update;

namespace Infinity.App;

/// <summary>
/// OD-119: the editorial news of <c>anarqz/InfinityNews</c> (docs/contracts/NEWS_FEED.md), on the right of PLAY (below it at the Deck's
/// size and smaller) and on the News page. Read from raw.githubusercontent.com only, so it loads signed out and offline-first: the last
/// good copy at once, then a refresh. Server notices stay the operational channel after sign-in (unchanged).
/// </summary>
public sealed partial class ShellViewModel
{
    private NewsClient? _newsClient;
    private NewsReadState? _newsRead;
    private NewsSnapshot? _newsSnapshot;
    private bool _newsLoading, _newsLoaded, _onNews, _newsFromHome, _wideLayout = true;
    private NewsCard? _newsOpen;
    private FlowDocument? _newsDocument;
    /// <summary>The harness's transport (a synthetic feed); production builds the real one.</summary>
    internal HttpMessageHandler? NewsHandler { get; set; }
    /// <summary>The harness's own folder for the news cache and read state (never the player's).</summary>
    internal string? NewsStateDirectory { get; set; }

    public ObservableCollection<NewsCard> NewsCards { get; } = [];

    /// <summary>The feed's address: the official one; a development build may point FI_DEV_NEWS_BASE_URL at a local fixture folder.</summary>
    private Uri NewsBase
    {
        get
        {
            if (!_config.Production && Environment.GetEnvironmentVariable("FI_DEV_NEWS_BASE_URL") is { Length: > 0 } text)
            {
                if (Directory.Exists(text)) return new Uri(Path.GetFullPath(text).TrimEnd('\\', '/') + Path.DirectorySeparatorChar);
                if (Uri.TryCreate(text.EndsWith('/') ? text : text + "/", UriKind.Absolute, out var url)) return url;
            }
            return NewsClient.OfficialBase;
        }
    }

    private NewsClient News => _newsClient ??= new NewsClient(NewsBase, Path.Combine(NewsStateDirectory ?? _stateDirectory, "news"), NewsHandler);
    private NewsReadState NewsRead => _newsRead ??= new NewsReadState(NewsStateDirectory ?? _stateDirectory);

    // Pages and layout ------------------------------------------------------------------------------------------------

    public bool ShowNews => _onNews && !_onMods && !_onSetup && !_onBugReport && !_onAccount && !ShowCrashReport && !ShowPickSave;
    public bool ShowNewsList => ShowNews && _newsOpen is null;
    public bool ShowNewsDetail => ShowNews && _newsOpen is not null;

    /// <summary>The PLAY column: beside PLAY on a wide window; below it at 1280x800 and smaller, so PLAY is never squeezed.</summary>
    public bool ShowNewsColumn => NewsCards.Count > 0 || NewsStatus.Length > 0;
    public bool NewsBeside => _wideLayout && ShowNewsColumn;
    public bool NewsBelow => !_wideLayout && ShowNewsColumn;
    /// <summary>The PLAY column shows the newest few; the News page shows them all.</summary>
    public IEnumerable<NewsCard> NewsPreview => NewsCards.Take(4);
    public bool NewsHasMore => NewsCards.Count > 4;

    /// <summary>The window's width decides where the column goes (MainWindow, on resize).</summary>
    public void SetWideLayout(bool wide)
    {
        if (_wideLayout == wide) return;
        _wideLayout = wide;
        Changed(nameof(NewsBeside)); Changed(nameof(NewsBelow));
    }

    public void OpenNews()
    {
        LeaveAccount(); _onMods = _onSetup = _onBugReport = false; _crashOpen = false;
        _onNews = true; _newsOpen = null; _newsDocument = null;
        Changed(string.Empty);
        if (!_newsLoaded) _ = LoadNewsAsync();
    }

    public async Task OpenNewsItemAsync(NewsCard card, bool fromHome)
    {
        ArgumentNullException.ThrowIfNull(card);
        LeaveAccount(); _onMods = _onSetup = _onBugReport = false; _crashOpen = false;
        _onNews = true; _newsFromHome = fromHome; _newsOpen = card; _newsDocument = null;
        NewsRead.MarkRead(card.Item.Id); card.Unread = false;
        Changed(string.Empty);
        NewsMarkdown? markdown = null;
        try { markdown = await News.MarkdownAsync(card.Item, S.Locale, CancellationToken.None); }
        catch (Exception error) when (NewsFailure(error)) { Log("news: markdown failed: " + error.GetType().Name); }
        if (!ReferenceEquals(_newsOpen, card)) return; // another item was opened meanwhile
        var item = card.Item;
        _newsDocument = NewsRenderer.Render(markdown?.Text ?? card.Summary, path => AssetAsync(item, path), NewsRenderer.OpenInBrowser,
            Application.Current.Resources);
        Changed(nameof(NewsDocument));
    }

    public void NewsBack()
    {
        if (_newsFromHome) { _onNews = false; _newsOpen = null; _newsDocument = null; _newsFromHome = false; Changed(string.Empty); return; }
        _newsOpen = null; _newsDocument = null; Changed(string.Empty);
    }

    public NewsCard? NewsOpen => _newsOpen;
    public FlowDocument? NewsDocument => _newsDocument;

    // The feed -----------------------------------------------------------------------------------------------------------

    /// <summary>Shown under the news: loading, nothing yet, or the date of the last good copy when the refresh failed.</summary>
    public string NewsStatus => _newsSnapshot is null
        ? _newsLoading || !_newsLoaded ? S["news.loading"] : S["news.failed"]
        : _newsSnapshot.Stale ? S.Format("news.stale", ("date", _newsSnapshot.AsOf.ToLocalTime().ToString("g", Culture)))
        : NewsCards.Count == 0 ? S["news.empty"] : "";
    public bool HasNewsStatus => NewsStatus.Length > 0;
    public bool NewsUnread => NewsCards.Any(c => c.Unread);

    private CultureInfo Culture => CultureInfo.GetCultureInfo(S.Locale.StartsWith("pt", StringComparison.OrdinalIgnoreCase) ? "pt-BR" : "en-US");

    /// <summary>The last good copy at once, then the network; never throws (a bad file never crashes the launcher).</summary>
    public async Task LoadNewsAsync()
    {
        if (_newsLoading) return;
        _newsLoading = true; NewsChanged();
        try
        {
            var client = News;
            if (_newsSnapshot is null && await Task.Run(client.Cached) is { } cached) ShowFeed(cached);
            var fresh = await client.RefreshAsync(CancellationToken.None);
            if (fresh is not null) ShowFeed(fresh);
            if (client.LastError is { } why) Log("news: refresh failed (" + why + ")" + (fresh is null ? "" : ", showing the copy from " + fresh.AsOf.ToString("u", CultureInfo.InvariantCulture)));
        }
        catch (Exception error) when (NewsFailure(error)) { Log("news: unavailable: " + error.GetType().Name); }
        finally { _newsLoading = false; _newsLoaded = true; NewsChanged(); }
    }

    private static bool NewsFailure(Exception error) => error is HttpRequestException or IOException or InvalidDataException or UnauthorizedAccessException
        or OperationCanceledException or JsonException or ArgumentException or NotSupportedException;

    private void ShowFeed(NewsSnapshot snapshot)
    {
        _newsSnapshot = snapshot;
        if (snapshot.Feed.Skipped > 0) Log("news: " + snapshot.Feed.Skipped.ToString(CultureInfo.InvariantCulture) + " item(s) skipped (no id or title)");
        var launcher = LauncherVersion.TryParse(Version, out var parsed) ? parsed : null;
        var items = snapshot.Feed.For(launcher);
        var known = NewsCards.ToDictionary(c => c.Item.Id, StringComparer.Ordinal);
        NewsCards.Clear();
        foreach (var item in items)
        {
            var card = known.TryGetValue(item.Id, out var old) && old.Item == item ? old
                : new NewsCard(item, item.Title.For(S.Locale), item.Summary.For(S.Locale), item.Day?.ToString("d MMM yyyy", Culture) ?? item.Date, !NewsRead.IsRead(item.Id), S["news.pinned"]);
            NewsCards.Add(card);
            if (card.Cover is null && item.Cover is { } cover) _ = LoadCoverAsync(card, cover);
        }
        NewsChanged();
    }

    private async Task LoadCoverAsync(NewsCard card, string path)
    {
        if (await AssetAsync(card.Item, path) is { Length: > 0 } bytes && Bitmap(bytes) is { } image) card.Cover = image;
    }

    private async Task<byte[]?> AssetAsync(NewsItem item, string path)
    {
        try { return await News.AssetAsync(item, path, CancellationToken.None); }
        catch (Exception error) when (NewsFailure(error)) { return null; }
    }

    private static BitmapImage? Bitmap(byte[] bytes)
    {
        try
        {
            var bitmap = new BitmapImage();
            using (var stream = new MemoryStream(bytes)) { bitmap.BeginInit(); bitmap.CacheOption = BitmapCacheOption.OnLoad; bitmap.DecodePixelWidth = 720; bitmap.StreamSource = stream; bitmap.EndInit(); }
            bitmap.Freeze();
            return bitmap;
        }
        catch (Exception error) when (error is IOException or NotSupportedException or InvalidOperationException or ArgumentException or FileFormatException) { return null; }
    }

    private void NewsChanged()
    {
        foreach (var name in new[] { nameof(NewsStatus), nameof(HasNewsStatus), nameof(ShowNewsColumn), nameof(NewsBeside), nameof(NewsBelow),
            nameof(NewsPreview), nameof(NewsHasMore), nameof(NewsUnread) }) Changed(name);
    }
}

/// <summary>One news card: the item, its texts in the player's language, its cover once loaded and whether it is unread.</summary>
public sealed class NewsCard(NewsItem item, string title, string summary, string date, bool unread, string pinnedLabel) : INotifyPropertyChanged
{
    private ImageSource? _cover;
    private bool _unread = unread;
    public NewsItem Item { get; } = item;
    public string Title { get; } = title;
    public string Summary { get; } = summary;
    public string Date { get; } = date;
    public bool Pinned => Item.Pinned;
    public string PinnedLabel { get; } = pinnedLabel;
    public ImageSource? Cover { get => _cover; set { _cover = value; Changed(); Changed(nameof(HasCover)); } }
    public bool HasCover => _cover is not null;
    public bool Unread { get => _unread; set { _unread = value; Changed(); } }

    public event PropertyChangedEventHandler? PropertyChanged;
    private void Changed([CallerMemberName] string? name = null) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}
