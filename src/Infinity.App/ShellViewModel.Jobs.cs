// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Globalization;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>One bottom-right toast (owner 2026-11-10): a Background job's label, its small spinner or progress, its end.</summary>
public sealed record JobToast(Job Job, string Label, string Detail, bool Running, bool Done, bool Failed, double ProgressPercent, bool HasProgress)
{
    public bool HasDetail => Detail.Length > 0;
    public bool CanCancel => Running && Job.Spec.Cancellable;
    public bool Indeterminate => Running && !HasProgress;
}

/// <summary>
/// Owner 2026-11-10: "Users should not get blocked from interacting with the launcher while the mod catalog is loading. We need a kind of a
/// toast that appears on bottom-right of the screen showing the queue of small jobs happening in background. SOME loads must lock the
/// screen [...]. Others must appear as an overlay in the center of the screen [...]. These 'non-blocking' loadings [...] must be configured
/// as 'should_block_play'". Every job goes through <see cref="Jobs"/>; Lock and Centre jobs open the busy overlay, Background jobs are
/// <see cref="JobToasts"/>, and the running ones that hold PLAY are named on PLAY (<see cref="PlayWaitsText"/>).
/// </summary>
public sealed partial class ShellViewModel
{
    private JobRunner? _jobRunner;
    private string _playWaitsKey = string.Empty;

    /// <summary>Past this many toasts the stack collapses to "N tasks running".</summary>
    internal const int ToastsShown = 3;

    internal JobRunner Jobs
    {
        get
        {
            if (_jobRunner is { } runner) return runner;
            runner = _jobRunner = new JobRunner(
                (presentation, label) => presentation == JobPresentation.Lock ? LockScreen(label) : Loading(label),
                error =>
                {
                    Log("jobs: failed: " + ErrorChain(error));
                    return error is JobFailedException failed ? failed.Message
                        : error is Infinity.Core.Api.ApiException or System.Net.Http.HttpRequestException ? Describe(error) : S["jobs.failed"];
                });
            runner.Changed += OnJobsChanged;
            runner.Ended += _ => { if (CanSettle && InShell) { FillPlayBoard(); Changed(string.Empty); } };
            return runner;
        }
    }

    public ObservableCollection<JobToast> JobToasts { get; } = [];

    public bool ShowJobToasts => JobToasts.Count > 0;

    /// <summary>"+2 more tasks running" under the shown toasts, when the stack is collapsed.</summary>
    public string JobsMoreText { get; private set; } = string.Empty;

    public bool HasJobsMore => JobsMoreText.Length > 0;

    /// <summary>A running job holds PLAY (ShouldBlockPlay, or a Lock/Centre job).</summary>
    public bool JobsBlockPlay => _jobRunner?.BlockingPlay.Count > 0;

    /// <summary>PLAY's note while it waits: "Waiting for: Moving your mods…".</summary>
    public string PlayWaitsText => _jobRunner?.BlockingPlay is { Count: > 0 } jobs
        ? S.Format("jobs.play_waits", ("jobs", string.Join(", ", jobs.Select(j => j.Label)))) : string.Empty;

    /// <summary>A screen lock (bulk mod changes, a folder move): the overlay, and every button's <see cref="Idle"/> gate closed.</summary>
    private Releaser LockScreen(string label)
    {
        var scope = Loading(label);
        _lockDepth++;
        Changed(nameof(BusyWhy));
        return new Releaser(() => { _lockDepth--; scope.Dispose(); });
    }

    private int _lockDepth;

    private sealed class Releaser(Action release) : IDisposable
    {
        private Action? _release = release;
        public void Dispose() { var r = _release; _release = null; r?.Invoke(); }
    }

    private void OnJobsChanged()
    {
        // running jobs first, then failures (they wait for Retry), then the checks of finished ones
        var toasts = Jobs.Toasts.OrderBy(j => j.State == JobState.Running ? 0 : j.State == JobState.Failed ? 1 : 2).ToList();
        // owner 2026-11-10: in place (a toast whose line did not change keeps its drawing and its spinner)
        var shown = new List<JobToast>();
        foreach (var job in toasts.Take(ToastsShown))
        {
            var detail = job.State == JobState.Failed ? job.Error ?? S["native.failed"] : job.ProgressText ?? string.Empty;
            shown.Add(new JobToast(job, job.Label, detail, job.Running, job.State is JobState.Succeeded, job.State == JobState.Failed,
                (job.Progress ?? 0) * 100, job.Progress is not null));
        }
        ListSync.Sync(JobToasts, shown, (a, b) => a.Equals(b), (a, b) => ReferenceEquals(a.Job, b.Job));
        var more = toasts.Count - ToastsShown;
        JobsMoreText = more > 0 ? S.Format("jobs.more", ("count", more.ToString(CultureInfo.CurrentCulture))) : string.Empty;
        Changed(nameof(ShowJobToasts));
        Changed(nameof(JobsMoreText));
        Changed(nameof(HasJobsMore));
        Changed(nameof(JobsBlockPlay));
        Changed(nameof(PlayWaitsText));
        Changed(nameof(CanPlay));
        Changed(nameof(CanPlayOffline));
        // PLAY's buttons carry their enabled state: rebuilt when the set of jobs holding PLAY changes
        var key = string.Join("|", Jobs.BlockingPlay.Select(j => j.Id)) + "|" + string.Join("|", _uploading.Keys);
        if (key != _playWaitsKey) { _playWaitsKey = key; if (InShell) FillPlayBoard(); }
    }

    /// <summary>
    /// <paramref name="work"/> as a job; its failure stays in the toast (Retry), and is also thrown here so the caller's own handling
    /// (a retry, a message) still runs. A second start with the same key joins the running one, and sees its real outcome: the shared
    /// job's failure is thrown to every caller (<see cref="JobRunner.RunOrThrowAsync"/>), a timeout (OperationCanceledException) included.
    /// </summary>
    private Task RunJobOrThrowAsync(string key, string label, JobPresentation presentation, bool blocksPlay, Func<JobContext, Task> work) =>
        Jobs.RunOrThrowAsync(new JobSpec(key, label, presentation, blocksPlay, work));

    /// <summary>A Background job whose failure only shows in its toast (with Retry).</summary>
    internal Job StartBackgroundJob(string key, string label, bool blocksPlay, Func<JobContext, Task> work, bool cancellable = false) =>
        Jobs.Start(new JobSpec(key, label, JobPresentation.Background, blocksPlay, work, cancellable));

    /// <summary>The explicit "check for updates": a background toast (the result shows on its own screen when it needs one).</summary>
    public Task CheckLauncherUpdatesInBackgroundAsync() =>
        StartBackgroundJob("update-check", S["jobs.update_check"], blocksPlay: false, _ => CheckLauncherUpdatesAsync()).Completion;

    public void RetryJob(JobToast toast) { ArgumentNullException.ThrowIfNull(toast); Log("jobs: retry " + toast.Job.Key); _ = toast.Job.Retry(); }

    public void DismissJob(JobToast toast) { ArgumentNullException.ThrowIfNull(toast); Log("jobs: dismissed " + toast.Job.Key); toast.Job.Dismiss(); }

    public void CancelJob(JobToast toast) { ArgumentNullException.ThrowIfNull(toast); Log("jobs: cancel " + toast.Job.Key); toast.Job.Cancel(); }

    // ------------------------------------------------------------------ the cloud save upload (owner 2026-11-10)

    /// <summary>Uploads running now, per (game, XUID): their toast's end.</summary>
    private readonly Dictionary<(string Game, ulong Xuid), TaskCompletionSource> _uploading = [];

    /// <summary>
    /// Owner 2026-11-10: "After quitting the game, the launcher stays freezed for a moment while the cloud save is updating, this should go
    /// into the [toast] with a small loader as well" and, first-hand later that day, "We should prevent any play from happening while
    /// cloud saves are being uploaded": a toast for each upload (on the UI thread, from the save steps) marked should_block_play, so no
    /// profile of any game plays until every upload has ended (<see cref="UploadingAny"/>; PLAY says "Uploading cloud save…").
    /// </summary>
    private void TrackSaveUpload(string game, ulong xuid, Infinity.Core.Saves.SaveStep step)
    {
        if (step == Infinity.Core.Saves.SaveStep.Uploading)
        {
            if (_uploading.ContainsKey((game, xuid))) return;
            var done = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
            _uploading[(game, xuid)] = done;
            StartBackgroundJob("upload|" + game + "|" + xuid.ToString(CultureInfo.InvariantCulture), S["jobs.uploading_save"], blocksPlay: true,
                _ => done.Task);
            OnJobsChanged();
            return;
        }
        if (step is Infinity.Core.Saves.SaveStep.Uploaded or Infinity.Core.Saves.SaveStep.Kept or Infinity.Core.Saves.SaveStep.Refused
            or Infinity.Core.Saves.SaveStep.SeedAccepted or Infinity.Core.Saves.SaveStep.SeedRefused or Infinity.Core.Saves.SaveStep.RestoreRefused)
            EndSaveUpload(game, xuid);
    }

    private void EndSaveUpload(string game, ulong xuid)
    {
        if (!_uploading.Remove((game, xuid), out var done)) return;
        done.TrySetResult();
        OnJobsChanged();
    }

    /// <summary>The session ended: any upload it was still reporting is over (the session's own result says how).</summary>
    private void FinishSaveUploadJob()
    {
        foreach (var key in _uploading.Keys.ToArray()) EndSaveUpload(key.Game, key.Xuid);
    }

    /// <summary>A cloud save upload is running (any profile, any game): every PLAY waits for it (owner 2026-11-10).</summary>
    private bool UploadingAny => _uploading.Count > 0;

    /// <summary>
    /// Why PLAY waits for a job now, or null: an upload first ("Uploading cloud save…"), else the should_block_play jobs by name. For the
    /// play paths' own refusals (a click that got past a button drawn before the job started).
    /// </summary>
    private string? PlayHeldByJobs => UploadingAny ? S["jobs.play_uploading"] : JobsBlockPlay ? PlayWaitsText : null;
}
