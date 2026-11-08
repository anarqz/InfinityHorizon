// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Overlay;

namespace Infinity.Core.Shell;

/// <summary>
/// OD-211 (CR-S102, CORE_API §4.2a): the friends' Discord avatars, one cache for the launcher's FRIENDS sidebar and the overlay's
/// (0x2D <see cref="UiFrame.FriendAvatar"/>). An image is fetched only from <c>cdn.discordapp.com</c> over https
/// (<see cref="DiscordLinks.Avatar"/>), once per URL: never per click or per poll. It is decoded to <see cref="UiFrame.AvatarSide"/> x
/// <see cref="UiFrame.AvatarSide"/> straight-alpha RGBA by <c>decode</c> (the app's image decoder; null when it does not decode) and
/// kept in memory, at most <see cref="MaxEntries"/> images (the oldest read goes first). A URL that failed is not fetched again for
/// <see cref="RetryFailedAfter"/>. At most <see cref="MaxConcurrent"/> fetches run at once. Never logs a URL (it names a Discord user).
/// </summary>
public sealed class FriendAvatars(Func<Uri, CancellationToken, Task<byte[]?>> fetch, Func<byte[], byte[]?> decode,
    TimeProvider? clock = null, Action<string>? log = null) : IDisposable
{
    public const int MaxEntries = 128, MaxConcurrent = 2;

    public static readonly TimeSpan RetryFailedAfter = TimeSpan.FromMinutes(10);

    /// <summary>How long one fetch may take.</summary>
    public static readonly TimeSpan FetchBudget = TimeSpan.FromSeconds(15);

    private readonly Func<Uri, CancellationToken, Task<byte[]?>> _fetch = fetch ?? throw new ArgumentNullException(nameof(fetch));
    private readonly Func<byte[], byte[]?> _decode = decode ?? throw new ArgumentNullException(nameof(decode));
    private readonly TimeProvider _clock = clock ?? TimeProvider.System;
    private readonly Action<string> _log = log ?? (_ => { });
    private readonly object _lock = new();
    private readonly Dictionary<string, (byte[] Rgba, long Used)> _images = new(StringComparer.Ordinal);
    private readonly Dictionary<string, long> _failed = new(StringComparer.Ordinal);
    private readonly HashSet<string> _loading = new(StringComparer.Ordinal);
    private readonly SemaphoreSlim _slots = new(MaxConcurrent, MaxConcurrent);
    private long _tick;

    /// <summary>An image finished loading (its URL): the sidebars draw it now.</summary>
    public event Action<string>? Loaded;

    /// <summary>The fetches started so far (tests).</summary>
    public int Fetches { get; private set; }

    /// <summary>Whether <paramref name="url"/> may be fetched at all: https, default port, no user info, host <c>cdn.discordapp.com</c>.</summary>
    public static bool Allowed(string? url) => DiscordLinks.Avatar(url) is not null;

    /// <summary>The decoded RGBA of <paramref name="url"/> when loaded, else null (and nothing is fetched: see <see cref="Want"/>).</summary>
    public byte[]? Get(string? url)
    {
        if (url is null) return null;
        lock (_lock)
        {
            if (!_images.TryGetValue(url, out var image)) return null;
            _images[url] = (image.Rgba, ++_tick);
            return image.Rgba;
        }
    }

    /// <summary>
    /// Starts loading <paramref name="url"/> unless it is loaded, loading, failed recently or not allowed; returns the task (tests wait for
    /// it), completed at once when nothing starts.
    /// </summary>
    public Task Want(string? url)
    {
        if (DiscordLinks.Avatar(url) is not { } uri) return Task.CompletedTask;
        lock (_lock)
        {
            if (_images.ContainsKey(url!) || _loading.Contains(url!)) return Task.CompletedTask;
            if (_failed.TryGetValue(url!, out var at) && _clock.GetElapsedTime(at) < RetryFailedAfter) return Task.CompletedTask;
            _failed.Remove(url!);
            _loading.Add(url!);
            Fetches++;
        }
        return Task.Run(() => LoadAsync(url!, uri));
    }

    public void Dispose() => _slots.Dispose();

    private async Task LoadAsync(string url, Uri uri)
    {
        byte[]? rgba = null;
        await _slots.WaitAsync().ConfigureAwait(false);
        try
        {
            using var budget = new CancellationTokenSource(FetchBudget, _clock);
            var bytes = await _fetch(uri, budget.Token).ConfigureAwait(false);
            if (bytes is { Length: > 0 }) rgba = _decode(bytes);
            if (rgba is not null && rgba.Length != UiFrame.AvatarSide * UiFrame.AvatarSide * 4) rgba = null;
            if (rgba is null) _log("friends: a Discord avatar did not load (no image, or it does not decode)");
        }
        catch (Exception e) when (e is HttpRequestException or IOException or OperationCanceledException or TimeoutException or InvalidDataException
            or NotSupportedException or ArgumentException or InvalidOperationException)
        {
            _log("friends: a Discord avatar did not load (" + e.GetType().Name + ")");
        }
        finally
        {
            _slots.Release();
        }
        lock (_lock)
        {
            _loading.Remove(url);
            if (rgba is null)
            {
                _failed[url] = _clock.GetTimestamp();
                return;
            }
            if (_images.Count >= MaxEntries)
            {
                var oldest = _images.MinBy(i => i.Value.Used).Key;
                _images.Remove(oldest);
            }
            _images[url] = (rgba, ++_tick);
        }
        Loaded?.Invoke(url);
    }
}
