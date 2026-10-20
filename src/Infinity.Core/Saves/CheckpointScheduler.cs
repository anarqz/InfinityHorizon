// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;

namespace Infinity.Core.Saves;

/// <summary>
/// When to commit a checkpoint while the game runs (SAVES_API.md §4.4, SAVE_SYNC.md §5), from the
/// server's <see cref="CheckpointPolicy"/>: the newest sealed revision, only if its content digest
/// differs from the last committed one, once it has stayed the same for <see cref="SettleTime"/>, at
/// least <c>minIntervalSecs</c> after the previous commit, and at most <c>maxCommitsPerHour</c> − 2
/// checkpoints in any hour (any hour window, so no fixed session hour exceeds it either), keeping
/// room for the final commit. Pure: the caller feeds times and digests.
/// </summary>
public sealed class CheckpointScheduler
{
    /// <summary>Commits kept free in every hour for the final commit (and one retry).</summary>
    public const int Reserve = 2;

    private readonly CheckpointPolicy _policy;
    private readonly Queue<DateTimeOffset> _checkpoints = new();
    private readonly HashSet<string> _refused = new(StringComparer.Ordinal);
    private DateTimeOffset _lastCommit;
    private string? _lastContent;
    private string? _observed;
    private DateTimeOffset _observedSince;

    /// <param name="policy">From <c>GET /launcher/v1/info</c>.</param>
    /// <param name="sessionStart">The pre-launch sync counts as the previous commit.</param>
    /// <param name="committedContent">The content the cloud has at launch (the mark), or null.</param>
    public CheckpointScheduler(CheckpointPolicy policy, DateTimeOffset sessionStart, string? committedContent)
    {
        _policy = policy ?? throw new ArgumentNullException(nameof(policy));
        _lastCommit = sessionStart;
        _lastContent = committedContent;
    }

    /// <summary>How long a sealed revision must stay unchanged (SAVES_API.md §4.4: 5 s).</summary>
    public TimeSpan SettleTime { get; init; } = TimeSpan.FromSeconds(5);

    /// <summary>Why checkpoints stopped for the session (<c>SAVES.REVISION_CONFLICT</c>, <c>SAVES.LEASE_LOST</c>), or null.</summary>
    public string? StoppedBecause { get; private set; }

    /// <summary>Whether to commit the newest sealed revision, whose content digest is <paramref name="content"/>, now.</summary>
    public bool ShouldCommit(DateTimeOffset now, string? content)
    {
        if (!_policy.Enabled || StoppedBecause is not null || content is null)
        {
            return false;
        }
        if (content != _observed)
        {
            _observed = content;
            _observedSince = now;
        }
        while (_checkpoints.Count > 0 && now - _checkpoints.Peek() >= TimeSpan.FromHours(1))
        {
            _checkpoints.Dequeue();
        }
        return content != _lastContent
            && !_refused.Contains(content)
            && now - _observedSince >= SettleTime
            && now - _lastCommit >= TimeSpan.FromSeconds(_policy.MinIntervalSecs)
            && _checkpoints.Count < Math.Max(0, (int)_policy.MaxCommitsPerHour - Reserve);
    }

    /// <summary>A checkpoint was sent (whatever the answer, it counts against the hour).</summary>
    public void Sent(DateTimeOffset now)
    {
        _checkpoints.Enqueue(now);
        _lastCommit = now;
    }

    /// <summary>The cloud now holds <paramref name="content"/> (a 2xx checkpoint or any other confirmed commit).</summary>
    public void Committed(DateTimeOffset now, string content)
    {
        _lastCommit = now;
        _lastContent = content;
    }

    /// <summary><c>SAVES.GATE_REFUSED</c> / <c>SUMMARY_UNREADABLE</c> / <c>BUNDLE_INVALID</c>: never this content again; later revisions are tried.</summary>
    public void Refused(string content) => _refused.Add(content);

    /// <summary>No more checkpoints this session; after the game exits the decision runs on a fresh head.</summary>
    public void Stop(string reason) => StoppedBecause ??= reason;
}
