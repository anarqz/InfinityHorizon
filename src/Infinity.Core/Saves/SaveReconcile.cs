// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Api;

namespace Infinity.Core.Saves;

/// <summary>What this PC last agreed with the cloud for one profile: a revision and its content digest (lower hex).</summary>
public sealed record SyncMark(ulong Revision, string Content);

/// <summary>The cloud head as the decision reads it: revision 0 is an empty line; <c>Content</c> null when empty or garbled.</summary>
public sealed record CloudHead(ulong Revision, string? Content)
{
    /// <summary>From the wire head; a digest that is not 64 hex digits reads as none (SAVE_SYNC.md §2).</summary>
    public static CloudHead From(SaveHead head)
    {
        ArgumentNullException.ThrowIfNull(head);
        return new CloudHead(head.Revision, SaveReconcile.NormalizeDigest(head.ContentSha256));
    }
}

/// <summary>The decision before starting the game (<c>fi_saves::reconcile::Decision</c>).</summary>
public abstract record SyncDecision
{
    private SyncDecision()
    {
    }

    /// <summary>No save anywhere: play; the first sealed revision becomes the first upload (<c>If-Match: "0"</c>).</summary>
    public sealed record Empty : SyncDecision;

    /// <summary>Local equals cloud: write the mark if it differs, then play.</summary>
    public sealed record InSync(ulong Revision) : SyncDecision;

    /// <summary>This PC has no save: download and install the head.</summary>
    public sealed record Download(ulong Revision) : SyncDecision;

    /// <summary>The cloud moved and this PC did not since the mark: download and install the head.</summary>
    public sealed record FastForward(ulong Revision) : SyncDecision;

    /// <summary>This PC moved and the cloud did not: commit with <c>If-Match: Expected</c>.</summary>
    public sealed record Upload(ulong Expected) : SyncDecision;

    /// <summary>
    /// The cloud line is empty and this PC has a save (<c>fi_saves</c>: a Noob first save, <c>If-Match: "0"</c>).
    /// Under OD-52/OD-60 an eligible (Hardcore) line is never empty once seeded, so the caller seeds and
    /// installs revision 1 instead of uploading (<see cref="CloudSaves.InstallHeadAsync"/>).
    /// </summary>
    public sealed record FirstUpload : SyncDecision;

    /// <summary>Both moved (or no mark and they differ): ask the player. Nothing is overwritten.</summary>
    public sealed record Conflict(ulong Cloud, ulong? Base) : SyncDecision;
}

/// <summary><c>fi_saves::reconcile::decide</c>, line for line, on lower-hex content digests.</summary>
public static class SaveReconcile
{
    /// <summary>A content digest as 64 lower-hex digits, or null when it is not one.</summary>
    public static string? NormalizeDigest(string? digest) =>
        digest is { Length: 64 } && digest.All(Uri.IsHexDigit) ? digest.ToLowerInvariant() : null;

    /// <param name="local">The newest sealed revision's content digest, or null when this PC has none.</param>
    /// <param name="mark">The stored mark; a damaged one (digest not 64 hex) decides as no mark, which can only give a Conflict.</param>
    /// <param name="cloud">The head.</param>
    public static SyncDecision Decide(string? local, SyncMark? mark, CloudHead cloud)
    {
        ArgumentNullException.ThrowIfNull(cloud);
        local = local is null ? null : NormalizeDigest(local) ?? throw new ArgumentException("the local digest is not 64 hex digits", nameof(local));
        if (mark is not null)
        {
            mark = NormalizeDigest(mark.Content) is { } markContent ? mark with { Content = markContent } : null;
        }
        var cloudContent = NormalizeDigest(cloud.Content);
        if (cloudContent is null || cloud.Revision == 0)
        {
            return (local, mark) switch
            {
                (null, _) => new SyncDecision.Empty(),
                // A mark on an empty line means the line was emptied: never upload over that without the player.
                (_, { Revision: > 0 } m) => new SyncDecision.Conflict(cloud.Revision, m.Revision),
                _ => new SyncDecision.FirstUpload(),
            };
        }
        if (local is null)
        {
            return new SyncDecision.Download(cloud.Revision);
        }
        if (local == cloudContent)
        {
            return new SyncDecision.InSync(cloud.Revision);
        }
        return mark switch
        {
            { } m when m.Revision == cloud.Revision && m.Content == cloudContent => new SyncDecision.Upload(cloud.Revision),
            { } m when m.Revision < cloud.Revision && m.Content == local => new SyncDecision.FastForward(cloud.Revision),
            { } m => new SyncDecision.Conflict(cloud.Revision, m.Revision),
            null => new SyncDecision.Conflict(cloud.Revision, null),
        };
    }
}
