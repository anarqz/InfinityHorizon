// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>How a page asks <see cref="RemoteCache"/> for a value.</summary>
public enum CacheRead
{
    /// <summary>Opening or returning to a page: the cached value when there is one (no request); a stale one is refreshed in the background.</summary>
    Cached,

    /// <summary>The page's own "Refresh": a request, at most once per <see cref="RemoteCache.RefreshThrottle"/> per key.</summary>
    Refresh,
}

/// <summary>
/// Owner 2026-11-08: "store remote information instead of always loading when clicking ... there's no reason to bomb the server". The
/// launcher's cache of remote reads (the cloud revisions of a career, the Discord link, the server lists...): navigating between pages
/// renders from it with no request. A key is fetched again only when an action of ours changed it (<see cref="Invalidate"/>,
/// <see cref="InvalidatePrefix"/> or <see cref="Set{T}"/> with the response), on sign-in/out (<see cref="Clear"/>), on the page's explicit
/// refresh (throttled), or in the background once older than <see cref="StaleAfter"/>. Game-scoped data puts the game in its key
/// (<see cref="Key"/>). Two reads of one key never run in parallel: the second shares the first's request. Failures are not cached.
/// UI-thread use (no locks); a <see cref="Clear"/> drops what an older generation's request brings back.
/// </summary>
public sealed class RemoteCache(Func<DateTimeOffset>? clock = null)
{
    private sealed class Entry
    {
        public object? Value;
        public bool HasValue;
        public DateTimeOffset FetchedAt;
        public Task? Pending;
    }

    private readonly Func<DateTimeOffset> _clock = clock ?? (() => DateTimeOffset.UtcNow);
    private readonly Dictionary<string, Entry> _entries = new(StringComparer.Ordinal);
    private int _generation;

    /// <summary>A cached value older than this is refreshed in the background (the page shows the cached one meanwhile).</summary>
    public TimeSpan StaleAfter { get; init; } = TimeSpan.FromMinutes(5);

    /// <summary>The page's explicit refresh asks the server at most this often per key.</summary>
    public TimeSpan RefreshThrottle { get; init; } = TimeSpan.FromSeconds(10);

    /// <summary>Requests sent (a test and log counter).</summary>
    public int Fetches { get; private set; }

    /// <summary>A key: its kind, then its scope (the game for game-scoped data, then the profile...).</summary>
    public static string Key(string kind, params string?[] scope) => kind + ":" + string.Join(":", scope.Select(s => s ?? "-"));

    public bool TryGet<T>(string key, out T value)
    {
        if (_entries.TryGetValue(key, out var entry) && entry.HasValue && entry.Value is T typed) { value = typed; return true; }
        value = default!;
        return false;
    }

    /// <summary>
    /// The value of <paramref name="key"/>: cached (no request) unless missing, or the explicit refresh past its throttle. A stale cached value is
    /// returned at once and refreshed in the background; <paramref name="refreshed"/> receives the new value then.
    /// </summary>
    public async Task<T> GetAsync<T>(string key, Func<Task<T>> fetch, CacheRead read = CacheRead.Cached, Action<T>? refreshed = null)
    {
        ArgumentNullException.ThrowIfNull(fetch);
        var now = _clock();
        if (_entries.TryGetValue(key, out var entry) && entry.HasValue && entry.Value is T cached)
        {
            var age = now - entry.FetchedAt;
            if (read == CacheRead.Refresh ? age < RefreshThrottle : age < StaleAfter) return cached;
            if (read == CacheRead.Cached)
            {
                // stale: shown now, refreshed in the background without blocking the page
                _ = RefreshQuietlyAsync(key, fetch, refreshed);
                return cached;
            }
        }
        return await FetchAsync(key, fetch).ConfigureAwait(true);
    }

    private async Task RefreshQuietlyAsync<T>(string key, Func<Task<T>> fetch, Action<T>? refreshed)
    {
        try
        {
            var value = await FetchAsync(key, fetch).ConfigureAwait(true);
            refreshed?.Invoke(value);
        }
        catch (Exception e) when (e is not OutOfMemoryException)
        {
            // a background refresh that fails keeps the cached value; the next read tries again
        }
    }

    /// <summary>One request per key at a time: a read while one is pending shares it.</summary>
    private Task<T> FetchAsync<T>(string key, Func<Task<T>> fetch)
    {
        if (!_entries.TryGetValue(key, out var entry)) _entries[key] = entry = new Entry();
        if (entry.Pending is Task<T> pending) return pending;
        var generation = _generation;
        Fetches++;
        var task = Run();
        // a fetch that completed synchronously already stored its value and cleared nothing pending
        if (!task.IsCompleted) entry.Pending = task;
        return task;

        async Task<T> Run()
        {
            try
            {
                var value = await fetch().ConfigureAwait(true);
                // an Invalidate or Clear while it was in flight: the answer may predate the change, so it is returned but not kept
                if (generation == _generation && _entries.TryGetValue(key, out var current) && ReferenceEquals(current, entry))
                {
                    entry.Value = value;
                    entry.HasValue = true;
                    entry.FetchedAt = _clock();
                }
                return value;
            }
            finally
            {
                if (_entries.TryGetValue(key, out var current) && ReferenceEquals(current, entry)) entry.Pending = null;
            }
        }
    }

    /// <summary>Stores a value an action's response carried (no request needed to know it).</summary>
    public void Set<T>(string key, T value)
    {
        if (!_entries.TryGetValue(key, out var entry)) _entries[key] = entry = new Entry();
        entry.Value = value;
        entry.HasValue = true;
        entry.FetchedAt = _clock();
    }

    /// <summary>An action of ours changed <paramref name="key"/>: its next read asks the server.</summary>
    public void Invalidate(string key) => _entries.Remove(key);

    /// <summary>Every key starting with <paramref name="prefix"/> (a kind, or a kind and a game).</summary>
    public void InvalidatePrefix(string prefix)
    {
        foreach (var key in _entries.Keys.Where(k => k.StartsWith(prefix, StringComparison.Ordinal)).ToList()) _entries.Remove(key);
    }

    /// <summary>Sign-in, sign-out, an account or server switch: everything goes, and requests already sent store nothing.</summary>
    public void Clear()
    {
        _entries.Clear();
        _generation++;
    }

    public int Count => _entries.Count(e => e.Value.HasValue);
}
