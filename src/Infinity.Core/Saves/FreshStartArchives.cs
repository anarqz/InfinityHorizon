// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Api;

namespace Infinity.Core.Saves;

/// <summary>
/// One career a fresh start moved aside (<c>fi-launcher-archive\User_x\fresh-start-&lt;stamp&gt;</c>, the profile folder's own layout):
/// when (from the folder's name), its newest sealed revision, and the level and credits its ProfileData holds (null when not read).
/// </summary>
public sealed record FreshStartArchive(string Name, string Path, DateTime ArchivedUtc, LocalRevision Revision, int? Level, int? Credits);

/// <summary>A <c>fresh-start-*</c> folder the restore does not offer, and why.</summary>
public sealed record SkippedArchive(string Name, string Reason);

/// <summary>The profile's restorable careers, newest first, and the folders left out.</summary>
public sealed record FreshStartListing(IReadOnlyList<FreshStartArchive> Archives, IReadOnlyList<SkippedArchive> Skipped);

/// <summary>
/// OD-133: the careers CR-S51's fresh start moved aside on this PC for one profile. Only read: nothing here writes, moves or deletes an
/// archive folder.
/// </summary>
public static class FreshStartArchives
{
    public const string Prefix = "fresh-start-";
    private const string StampFormat = "yyyyMMdd'T'HHmmss'Z'";

    /// <summary><c>&lt;saves&gt;\fi-launcher-archive\User_&lt;XUID upper hex&gt;</c>.</summary>
    public static string ArchiveRoot(string savesRoot, ulong xuid)
    {
        ArgumentNullException.ThrowIfNull(savesRoot);
        return System.IO.Path.Combine(savesRoot, AzimuthSaves.ArchiveDir, AzimuthSaves.FolderFor(xuid));
    }

    /// <summary>The stamp a fresh start gives its folder (<c>yyyyMMddTHHmmssZ</c>), UTC.</summary>
    public static string Stamp(DateTime utc) => utc.ToUniversalTime().ToString(StampFormat, CultureInfo.InvariantCulture);

    /// <summary>The suffix of an empty marker folder: a fresh start that had no career to move aside (written outside sync-state.json).</summary>
    public const string MarkerSuffix = "-empty";

    /// <summary>A <c>fresh-start-&lt;stamp&gt;[-n]-empty</c> marker: it counts for the guard, never for the restore.</summary>
    public static bool IsMarker(string name) => name.EndsWith(MarkerSuffix, StringComparison.Ordinal);

    /// <summary>The <c>fresh-start-*</c> folder names of the profile, markers and any content (the once-per-line guard counts them all).</summary>
    public static IReadOnlyList<string> Names(string savesRoot, ulong xuid)
    {
        var root = ArchiveRoot(savesRoot, xuid);
        if (!Directory.Exists(root)) return [];
        return [.. Directory.EnumerateDirectories(root, Prefix + "*").Select(d => System.IO.Path.GetFileName(d)).Order(StringComparer.Ordinal)];
    }

    /// <summary>The names the restore may offer: <see cref="Names"/> without the empty markers.</summary>
    public static IReadOnlyList<string> ArchiveNames(string savesRoot, ulong xuid) => [.. Names(savesRoot, xuid).Where(n => !IsMarker(n))];

    /// <summary>The UTC instant in a <c>fresh-start-&lt;stamp&gt;[-n][-empty]</c> name, or null.</summary>
    public static DateTime? ArchivedAt(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        if (!name.StartsWith(Prefix, StringComparison.Ordinal)) return null;
        if (IsMarker(name)) name = name[..^MarkerSuffix.Length];
        var rest = name[Prefix.Length..];
        var stamp = rest.Length >= 16 ? rest[..16] : rest;
        var tail = rest[stamp.Length..];
        if (tail.Length > 0 && !(tail[0] == '-' && tail.Length > 1 && tail[1..].All(char.IsAsciiDigit))) return null;
        return DateTime.TryParseExact(stamp, StampFormat, CultureInfo.InvariantCulture, DateTimeStyles.AdjustToUniversal | DateTimeStyles.AssumeUniversal, out var at)
            ? DateTime.SpecifyKind(at, DateTimeKind.Utc) : null;
    }

    /// <summary>
    /// The profile's restorable careers, newest first: every <c>fresh-start-*</c> folder with a date in its name whose newest sealed
    /// revision is a complete Azimuth save revision that packs for the cloud. Every other folder is listed in
    /// <see cref="FreshStartListing.Skipped"/> with the reason. Never throws for the archive's contents.
    /// </summary>
    public static FreshStartListing List(string savesRoot, ulong xuid)
    {
        var archives = new List<FreshStartArchive>();
        var skipped = new List<SkippedArchive>();
        IReadOnlyList<string> names;
        try { names = ArchiveNames(savesRoot, xuid); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return new FreshStartListing([], [new SkippedArchive(ArchiveRoot(savesRoot, xuid), e.Message)]); }
        var root = ArchiveRoot(savesRoot, xuid);
        foreach (var name in names)
        {
            var path = System.IO.Path.Combine(root, name);
            if (ArchivedAt(name) is not { } at)
            {
                skipped.Add(new SkippedArchive(name, "the folder's name has no date"));
                continue;
            }
            try
            {
                if (AzimuthSaves.NewestSealed(path) is not { } revision)
                {
                    skipped.Add(new SkippedArchive(name, "no complete Azimuth save revision"));
                    continue;
                }
                CloudSaveFiles.Pack(revision); // the same checks the upload makes: known files only, sealed, within the bundle limit
                var (level, credits) = AzimuthSaves.Numbers(revision);
                archives.Add(new FreshStartArchive(name, path, at, revision, level, credits));
            }
            catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
            {
                skipped.Add(new SkippedArchive(name, e.Message));
            }
        }
        return new FreshStartListing([.. archives.OrderByDescending(a => a.ArchivedUtc).ThenByDescending(a => a.Name, StringComparer.Ordinal)], skipped);
    }
}

/// <summary>What a career restore came to.</summary>
public enum RestoreOutcomeKind
{
    /// <summary>Copied in and recorded as the profile's pending <c>upload</c> on top of the head; the next Play sends it under its lease.</summary>
    Recorded,

    /// <summary>The server's dry run said the upload would not pass: nothing changed on this PC (detail: code, rule and reasons).</summary>
    Refused,

    /// <summary>This server has no upload dry run (before CR-S71: 404/405): nothing was done.</summary>
    NotSupported,

    /// <summary>The line's first save is not in the cloud yet (head revision 0), or the line is lapsed: nothing was done.</summary>
    NotOffered,

    /// <summary>
    /// The career was copied in (it is now this PC's newest save) but it was not recorded for the cloud, or its final check failed:
    /// the next Play decides on it as on any progress on this PC. The backup from before the restore is in <see cref="RestoreOutcome.Backup"/>.
    /// </summary>
    InstalledNotQueued,

    /// <summary>Not done (detail: the code, and <see cref="RestoreOutcome.RetryAfter"/>): nothing changed on this PC's save.</summary>
    Failed,
}

/// <summary>
/// A career restore's outcome. <see cref="Backup"/>: the <c>before-restore-&lt;stamp&gt;</c> copy of the profile's folder (null when
/// nothing was touched); <see cref="Expected"/>: the head revision the pending upload goes on top of; <see cref="Rule"/>: the gate's rule
/// of a refusal (<c>*_CEILING</c>, <c>LIMITS</c>…).
/// </summary>
public sealed record RestoreOutcome(RestoreOutcomeKind Kind, string? Code = null, IReadOnlyList<string>? Reasons = null, string? Backup = null,
    ulong? Expected = null, ulong? LocalRevision = null, TimeSpan? RetryAfter = null, string? Rule = null);

/// <summary>
/// OD-133 "Restore a career from this PC's backups" (owner: only careers that pass the dry run are restored; the rest stay in their
/// backups): a career a fresh start moved aside goes back into the profile's folder and to the cloud as a normal <c>upload</c> on top of
/// the line's current head. The order:
/// <list type="number">
/// <item>the line's head is read (revision ≥ 1: the first save reached the cloud) and no upload of the profile is pending;</item>
/// <item>the picked archive's newest sealed revision is packed, read only, without touching the profile's folder;</item>
/// <item>the server's dry run (<c>POST .../save/upload-check</c>, SAVES_API.md §4.11, CR-S71, no lease) is asked whether that upload would
/// pass, with the very request the commit sends (<see cref="CloudSaves.DryRunUploadAsync"/>); anything but a pass changes nothing;</item>
/// <item>on a pass: the profile's folder is copied to <c>before-restore-&lt;stamp&gt;</c>, the archive's revision is installed as the
/// newest revision (<see cref="CloudSaveFiles.Install"/>: staged, verified, what it shadows archived too) and recorded as the pending
/// <c>upload</c> with <c>If-Match</c> = that head, marked as a restore (<see cref="PendingUpload.Restore"/>).</item>
/// </list>
/// The commit itself is the existing path: the next Play's <see cref="SessionSaves.BeforeSpawnAsync"/> sends a pending upload first,
/// under that session's lease and fence (the launcher holds no lease outside a Play), exactly as a CR-S52 seed. A head that moved meanwhile
/// is a REVISION_CONFLICT (bytes set aside, the conflict screen decides); a gate refusal there puts the <c>before-restore</c> save back.
/// The archive folder is only read. Only while the game is not running (the caller checks).
/// </summary>
public static class CareerRestore
{
    /// <summary>The restore is offered for a line whose first save is in the cloud (revision ≥ 1) and that is not lapsed.</summary>
    public static bool Offered(SaveHead? head) => head is { Revision: >= 1, LapsedSince: null };

    /// <summary>
    /// This PC's newest save is not the one the cloud has (the mark): unsent or parked progress, which the restore backs up and replaces.
    /// False when the PC has no save or it cannot be read.
    /// </summary>
    public static bool HasUnsentProgress(CloudSaves saves, string savesRoot, ulong xuid)
    {
        ArgumentNullException.ThrowIfNull(saves);
        try
        {
            if (AzimuthSaves.NewestSealed(CloudSaveFiles.ProfileDir(savesRoot, xuid)) is not { } newest) return false;
            return Fh2Bundle.ContentDigest(CloudSaveFiles.ReadFiles(newest)) != SaveReconcile.NormalizeDigest(saves.State(xuid)?.Mark?.Content);
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        {
            return false;
        }
    }

    /// <summary>A gate rule that means "grew faster than the line's history allows" (the one-step ceilings).</summary>
    public static bool IsCeiling(string? rule) => rule is not null && rule.EndsWith("_CEILING", StringComparison.Ordinal);

    public static async Task<RestoreOutcome> RestoreAsync(CloudSaves saves, ISaveUploadDryRun dryRun, string access, string savesRoot, ulong xuid,
        FreshStartArchive pick, DateTime nowUtc, Action<string>? note = null, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(saves);
        ArgumentNullException.ThrowIfNull(dryRun);
        ArgumentNullException.ThrowIfNull(pick);
        if (!dryRun.SupportsUploadDryRun)
        {
            note?.Invoke("restore: this client has no upload dry run: not offered");
            return new RestoreOutcome(RestoreOutcomeKind.NotSupported);
        }
        if (saves.State(xuid)?.PendingUpload is { } waiting)
        {
            note?.Invoke($"restore: upload {waiting.CommitId} of the profile is still pending: nothing done");
            return new RestoreOutcome(RestoreOutcomeKind.Failed, "LOCAL.UPLOADPENDING");
        }
        if (RestoreUndo.Owed(saves, savesRoot, xuid) is not null)
        {
            note?.Invoke("restore: an earlier restore's undo is still owed: nothing done");
            return new RestoreOutcome(RestoreOutcomeKind.Failed, RestoreUndo.OwedCode);
        }
        var (head, failure) = await saves.HeadAsync(access, xuid, cancellation).ConfigureAwait(false);
        if (head is null) return new RestoreOutcome(RestoreOutcomeKind.Failed, failure?.Code ?? ErrorCodes.SavesUnavailable, RetryAfter: failure?.RetryAfter);
        if (!Offered(head))
        {
            note?.Invoke($"restore: the line is at revision {head.Revision}{(head.LapsedSince is null ? "" : ", lapsed")}: not offered");
            return new RestoreOutcome(RestoreOutcomeKind.NotOffered);
        }

        // 2. the archive's save, read only
        PackedRevision packed;
        try
        {
            var revision = AzimuthSaves.ReadRevision(pick.Revision.Number, pick.Revision.Path)
                ?? throw new SaveException(pick.Name + ": the revision has no commit file any more");
            if (AzimuthSaves.Verify(revision) is { Count: > 0 } changed) throw new SaveException(pick.Name + ": " + string.Join("; ", changed));
            packed = CloudSaveFiles.Pack(revision);
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        {
            note?.Invoke("restore: the archive did not read: " + e.Message);
            return new RestoreOutcome(RestoreOutcomeKind.Failed, "LOCAL.LOCALUNREADABLE");
        }

        // 3. would the upload pass?
        UploadDryRunResult answer;
        try
        {
            answer = await CloudSaves.DryRunUploadAsync(dryRun, access, xuid, packed, head.Revision, CommitKinds.Upload, cancellation).ConfigureAwait(false);
        }
        catch (Exception e) when (e is HttpRequestException || (e is TaskCanceledException && !cancellation.IsCancellationRequested))
        {
            note?.Invoke("restore: the dry run was unreachable: " + e.GetType().Name);
            return new RestoreOutcome(RestoreOutcomeKind.Failed, "NETWORK");
        }
        note?.Invoke($"restore: dry run of {packed.ContentSha256[..12]} ({pick.Name}) on revision {head.Revision}: {answer.Verdict} {answer.Code} {answer.Rule} {string.Join(",", answer.Reasons)}");
        switch (answer.Verdict)
        {
            case UploadDryRunVerdict.Unsupported:
                return new RestoreOutcome(RestoreOutcomeKind.NotSupported);
            case UploadDryRunVerdict.Refused:
                return new RestoreOutcome(RestoreOutcomeKind.Refused, answer.Code, answer.Reasons, Expected: head.Revision, Rule: answer.Rule);
            case UploadDryRunVerdict.Error:
                return new RestoreOutcome(RestoreOutcomeKind.Failed, answer.Code, Expected: head.Revision, RetryAfter: answer.RetryAfter);
        }

        // 4. back up, copy in, record the upload
        var prepared = CloudSaves.PrepareFolders(savesRoot, xuid);
        if (prepared.Busy.Count > 0)
        {
            note?.Invoke("restore: a save of the profile is being written: nothing done");
            return new RestoreOutcome(RestoreOutcomeKind.Failed, "LOCAL.BUSY");
        }
        var stamp = FreshStartArchives.Stamp(nowUtc);
        var profile = CloudSaveFiles.ProfileDir(savesRoot, xuid);
        string? backup = null;
        InstalledRevision? installed = null;
        try
        {
            if (Directory.Exists(profile))
            {
                var root = FreshStartArchives.ArchiveRoot(savesRoot, xuid);
                backup = System.IO.Path.Combine(root, "before-restore-" + stamp);
                for (var n = 1; Directory.Exists(backup); n++) backup = System.IO.Path.Combine(root, $"before-restore-{stamp}-{n}");
                AzimuthSaves.CopyDir(profile, backup);
                note?.Invoke("restore: the profile's folder was copied to " + System.IO.Path.GetFileName(backup));
            }
            installed = CloudSaveFiles.Install(savesRoot, xuid, packed.Files, packed.ContentSha256, Guid.CreateVersion7().ToString("N"), "restore-shadowed-" + stamp);
            var newest = AzimuthSaves.NewestSealed(profile);
            if (newest is null || newest.Number != installed.Number) throw new SaveException("the restored revision is not the newest sealed one", landed: true);
            if (saves.RecordUpload(xuid, newest, head.Revision, CommitKinds.Upload,
                    restore: backup is null ? string.Empty : System.IO.Path.GetFileName(backup)) is { } notRecorded)
            {
                note?.Invoke("restore: copied in but not recorded: " + notRecorded.Code);
                return new RestoreOutcome(RestoreOutcomeKind.InstalledNotQueued, notRecorded.Code, Backup: backup, Expected: head.Revision, LocalRevision: installed.Number);
            }
            // the restore stays unsettled until its commit is confirmed, a conflict takes it, or an undo puts the earlier save back
            saves.SetRestore(xuid, new RestoreRecord(backup is null ? string.Empty : System.IO.Path.GetFileName(backup), head.Revision, packed.ContentSha256));
            note?.Invoke($"restore: {pick.Name} is revision {installed.Number:x16} and goes up as upload on top of {head.Revision} at the next Play");
            return new RestoreOutcome(RestoreOutcomeKind.Recorded, Backup: backup, Expected: head.Revision, LocalRevision: installed.Number);
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        {
            // landed in revisions (or installed, then a later step failed): the restored career is this PC's save now, not queued
            var swapped = installed is not null || (e is SaveException { Landed: true });
            note?.Invoke((swapped ? "restore: copied in but not queued: " : "restore: the copy failed: ") + e.Message);
            return new RestoreOutcome(swapped ? RestoreOutcomeKind.InstalledNotQueued : RestoreOutcomeKind.Failed, "LOCAL.INTEGRITY", Backup: backup,
                Expected: head.Revision, LocalRevision: installed?.Number);
        }
    }
}

/// <summary>
/// OD-133: putting this PC's save from before a restore back, when the restored career will never be committed (refused, any other
/// permanent failure but a conflict, recorded bytes gone, the player setting it aside, or an interruption between the set-aside and the
/// undo). The debt lives only in the sync state (<see cref="ProfileSyncState.Restore"/> with <see cref="RestoreRecord.OwedReason"/>),
/// written before anything is set aside. While it is owed the profile does not play (the launch stops with <see cref="OwedCode"/>), and
/// every launch tries again. Nothing is deleted: the earlier save is installed again from <c>before-restore-*</c> (staged and verified,
/// <see cref="CloudSaveFiles.Install"/>; what it shadows goes to <c>restore-shadowed-*</c>), or, without a usable backup, the cloud's
/// head is installed. When neither can be done the player may keep the restored career (<see cref="KeepRestored"/>).
/// </summary>
public static class RestoreUndo
{
    /// <summary>The code of a launch stopped, or a restore refused, while an undo is owed.</summary>
    public const string OwedCode = "LOCAL.RESTORE_UNDO_OWED";

    /// <summary>The owed undo, or null.</summary>
    public static RestoreRecord? Owed(CloudSaves saves, string savesRoot, ulong xuid)
    {
        ArgumentNullException.ThrowIfNull(saves);
        return saves.State(xuid)?.Restore is { OwedReason: not null } owed ? owed : null;
    }

    /// <summary>Records the debt (before anything is set aside): from now on the restored career is never sent.</summary>
    public static RestoreRecord Owe(CloudSaves saves, string savesRoot, ulong xuid, string reason, DateTime nowUtc, Action<string>? note = null)
    {
        ArgumentNullException.ThrowIfNull(saves);
        var pending = saves.State(xuid)?.PendingUpload;
        var current = saves.State(xuid)?.Restore
            ?? (pending is { Restore: { } b } ? new RestoreRecord(b, pending.Expected, pending.Content) : null)
            ?? new RestoreRecord(string.Empty, 0, string.Empty);
        var owed = current with { OwedReason = reason, OwedAtUtc = nowUtc.ToUniversalTime().ToString("O", CultureInfo.InvariantCulture) };
        saves.SetRestore(xuid, owed);
        note?.Invoke($"restore: the restored career will not be committed ({reason}): this PC's save from before it is owed back ({owed.Backup})");
        return owed;
    }

    /// <summary>
    /// Before a launch: an unsettled restore whose pending upload is gone was either committed (the mark is its revision and content:
    /// settled) or cut off between its set-aside and its undo (owed now). A restore upload recorded without its record gets one.
    /// </summary>
    public static void Reconcile(CloudSaves saves, string savesRoot, ulong xuid, DateTime nowUtc, Action<string>? note = null)
    {
        ArgumentNullException.ThrowIfNull(saves);
        var state = saves.State(xuid);
        var record = state?.Restore;
        if (state?.PendingUpload is { Restore: { } backup } pending)
        {
            if (record is null) saves.SetRestore(xuid, new RestoreRecord(backup, pending.Expected, pending.Content));
            return; // still waiting for the cloud
        }
        if (record is null || record.OwedReason is not null) return;
        // the mark past the restore's base: it was committed (and the line may have moved on since): settled, nothing undone
        if (state?.Mark is { } mark && mark.Revision > record.Expected)
        {
            saves.SetRestore(xuid, null);
            note?.Invoke($"restore: the line is at revision {mark.Revision}, past the restore's base {record.Expected}: settled");
            return;
        }
        Owe(saves, savesRoot, xuid, "LOCAL.INTERRUPTED", nowUtc, note);
    }

    /// <summary>
    /// Pays the owed undo: <c>before-restore-*</c>'s newest sealed revision installed again, else the cloud's <paramref name="head"/>.
    /// True when paid (or nothing was owed); false when neither worked (the debt stays, marked <see cref="RestoreRecord.UndoFailed"/>).
    /// </summary>
    public static async Task<bool> UndoAsync(CloudSaves saves, string access, string savesRoot, ulong xuid, SaveHead? head, DateTime nowUtc,
        Action<string>? note = null, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(saves);
        if (Owed(saves, savesRoot, xuid) is not { } owed) return true;
        var label = "restore-shadowed-" + FreshStartArchives.Stamp(nowUtc);
        var backupDir = owed.Backup.Length == 0 ? null : System.IO.Path.Combine(FreshStartArchives.ArchiveRoot(savesRoot, xuid), owed.Backup);
        // 1. read the backup (a failure here is "no usable backup")
        IReadOnlyList<SaveFile>? files = null;
        try
        {
            if (backupDir is not null && AzimuthSaves.NewestSealed(backupDir) is { } before) files = CloudSaveFiles.ReadFiles(before);
        }
        catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
        {
            note?.Invoke("restore: the backup from before the restore did not read: " + e.Message);
        }
        // 2. install it; only a failure of this install may fall back to the cloud's head
        if (files is not null)
        {
            try
            {
                var installed = CloudSaveFiles.Install(savesRoot, xuid, files, Fh2Bundle.ContentDigest(files), Guid.CreateVersion7().ToString("N"), label);
                note?.Invoke($"restore: undone: {owed.Backup} is revision {installed.Number:x16} again; the restored career stays archived");
                return Paid(saves, xuid, note); // the state write's failure never leads to the head fallback
            }
            catch (Exception e) when (e is SaveException or IOException or UnauthorizedAccessException or System.Text.DecoderFallbackException)
            {
                note?.Invoke("restore: the backup from before the restore did not install: " + e.Message);
            }
        }
        else
        {
            note?.Invoke("restore: no usable backup from before the restore" + (owed.Backup.Length == 0 ? "" : $" ({owed.Backup})"));
        }
        // 3. the cloud's head instead, only when the backup held nothing newer than the cloud: no backup folder at all, or a backup whose
        // content is the mark's or the head's. Unknown or newer progress (unsent, parked) is never replaced silently: the debt stays.
        var backupContent = files is null ? null : Fh2Bundle.ContentDigest(files);
        var nothingNewer = files is null
            ? backupDir is null || !Directory.Exists(backupDir)
            : backupContent == SaveReconcile.NormalizeDigest(head?.ContentSha256) || backupContent == SaveReconcile.NormalizeDigest(saves.State(xuid)?.Mark?.Content);
        if (!nothingNewer)
        {
            saves.SetRestore(xuid, owed with { UndoFailed = true });
            note?.Invoke("restore: not undone yet: the backup may hold progress newer than the cloud's head; the player decides");
            return false;
        }        string? why = null;
        if (head is null || head.Revision == 0 || head.IsCloudExpired())
        {
            why = "no cloud revision to install";
        }
        else if (saves.Decide(savesRoot, xuid, head) is { Failure: { } busy })
        {
            why = busy.Code;
        }
        else
        {
            try
            {
                var outcome = await saves.InstallHeadAsync(access, savesRoot, xuid, head, saves.Decide(savesRoot, xuid, head).LocalContent, label, cancellation).ConfigureAwait(false);
                if (outcome.Failure is null)
                {
                    note?.Invoke($"restore: undone from the cloud's revision {head.Revision}; the restored career stays archived");
                    return Paid(saves, xuid, note);
                }
                why = outcome.Failure.Code;
            }
            catch (Exception e) when (e is HttpRequestException or IOException or UnauthorizedAccessException)
            {
                why = e.GetType().Name;
            }
        }
        saves.SetRestore(xuid, owed with { UndoFailed = true });
        note?.Invoke("restore: not undone yet: " + why);
        return false;
    }

    /// <summary>
    /// The undo installed: a restore upload still recorded (a crash between the debt and its set-aside) is set aside in the same step,
    /// before anything can send it, then the record goes (the write retried once). A write that still fails leaves the debt owed: the
    /// next launch installs the backup again (harmless), never the cloud's head over it.
    /// </summary>
    private static bool Paid(CloudSaves saves, ulong xuid, Action<string>? note)
    {
        for (var attempt = 0; attempt < 2; attempt++)
        {
            try
            {
                if (saves.State(xuid)?.PendingUpload is { Restore: not null }) saves.SetAsidePending(xuid);
                saves.SetRestore(xuid, null);
                return true;
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException)
            {
                note?.Invoke("restore: the undo's state was not written: " + e.Message);
            }
        }
        return false;
    }

    /// <summary>
    /// "Keep this career on this PC", offered once an undo could not be paid: the debt and the record go; the restored career is then
    /// an ordinary local save under the normal rules (it may be refused and parked by the server later). Nothing is deleted.
    /// </summary>
    public static void KeepRestored(CloudSaves saves, ulong xuid, Action<string>? note = null)
    {
        ArgumentNullException.ThrowIfNull(saves);
        saves.SetRestore(xuid, null);
        note?.Invoke("restore: the player keeps the restored career on this PC; it follows the normal rules from now on");
    }

    /// <summary>What the player's "Set the restore aside" came to.</summary>
    public enum SetAsideResult
    {
        /// <summary>The restore had reached the cloud after all (its answer was lost): settled as committed, nothing undone.</summary>
        AlreadyCommitted,

        /// <summary>Set aside and undone: this PC's save from before the restore is back.</summary>
        Undone,

        /// <summary>Set aside but the undo could not be paid yet (owed; the player may keep the restored career).</summary>
        NotUndone,

        /// <summary>A save of the profile is being written: nothing done.</summary>
        Busy,

        /// <summary>The cloud's head could not be read, so whether the restore already landed is unknown: nothing done.</summary>
        HeadUnreadable,
    }

    /// <summary>
    /// The player's "Set the restore aside and put my previous save back". <paramref name="head"/> is read first by the caller: when it
    /// is the pending restore's commit (<c>Expected + 1</c> with its content), the restore reached the cloud and is settled as committed.
    /// Otherwise the debt is recorded, the upload set aside (its bytes kept in <c>pending\set-aside</c>) and the undo paid. Only while
    /// the game is not running (the caller checks).
    /// </summary>
    public static async Task<SetAsideResult> SetAsideAsync(CloudSaves saves, string access, string savesRoot, ulong xuid, SaveHead? head, DateTime nowUtc,
        Action<string>? note = null, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(saves);
        if (CloudSaves.PrepareFolders(savesRoot, xuid).Busy.Count > 0)
        {
            note?.Invoke("restore: a save of the profile is being written: not set aside");
            return SetAsideResult.Busy;
        }
        var pending = saves.State(xuid)?.PendingUpload;
        if (pending is { Restore: not null })
        {
            if (head is null)
            {
                note?.Invoke("restore: the cloud's head was not read: whether the restore already landed is unknown; nothing set aside");
                return SetAsideResult.HeadUnreadable;
            }
            if (head is { } h && h.Revision == pending.Expected + 1
                && SaveReconcile.NormalizeDigest(h.ContentSha256) is { } landed && landed == SaveReconcile.NormalizeDigest(pending.Content))
            {
                saves.ConfirmPendingCommitted(xuid, h.Revision);
                saves.SetRestore(xuid, null);
                note?.Invoke($"restore: it already reached the cloud as revision {h.Revision}: settled, nothing undone");
                return SetAsideResult.AlreadyCommitted;
            }
            Owe(saves, savesRoot, xuid, "LOCAL.SET_ASIDE", nowUtc, note); // owed before the set-aside: an interruption still pays it
            saves.SetAsidePending(xuid);
        }
        else if (Owed(saves, savesRoot, xuid) is null)
        {
            return SetAsideResult.Undone; // nothing to set aside or owe
        }
        return await UndoAsync(saves, access, savesRoot, xuid, head, nowUtc, note, cancellation).ConfigureAwait(false) ? SetAsideResult.Undone : SetAsideResult.NotUndone;
    }

    /// <summary>Whether the profile has a restore waiting for the cloud, or an undo owed: the way out applies.</summary>
    public static bool Unsettled(CloudSaves saves, string savesRoot, ulong xuid) =>
        saves.State(xuid)?.PendingUpload is { Restore: not null } || Owed(saves, savesRoot, xuid) is not null;

    /// <summary>An undo is owed and its last attempt could not pay it: "Keep this career on this PC" is offered.</summary>
    public static bool Stuck(CloudSaves saves, string savesRoot, ulong xuid) => Owed(saves, savesRoot, xuid) is { UndoFailed: true };
}