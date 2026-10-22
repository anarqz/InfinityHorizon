// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Security.Cryptography;
using Infinity.Core.Api;

namespace Infinity.Core.Update;

/// <summary>
/// Journalled portable-runtime replacement. A separate verified helper calls this only after the
/// launcher has exited. The verifier is supplied by the signed-release implementation, never by a local plan.
/// </summary>
public sealed class UpdateInstaller(IUpdatePlanVerifier verifier, Func<bool> supervisedGameActive,
    Action<UpdateInstallProgress>? progress = null)
{
    private static readonly string[] RuntimeDirectories = ["hook", "overlay", "i18n", "i18n-win", "licenses"];
    private static readonly HashSet<string> RuntimeNames = new(StringComparer.OrdinalIgnoreCase)
    {
        "infinity.exe", "infinity.exe.sig", "infinity-updater.exe", "infinity-updater.exe.sig",
        "infinity.png", "LICENSE", "NOTICE", "SHA256SUMS", "SHA256SUMS.sig", "development-key.txt",
        "hook/infinity.dll", "hook/infinity.dll.sig", "overlay/infinity-overlay.dll", "overlay/infinity-overlay.dll.sig",
        "i18n/en.json", "i18n/pt-BR.json", "i18n-win/en.json", "i18n-win/pt-BR.json",
        "licenses/BouncyCastle-LICENSE.md", "licenses/fh2-mods-LICENSE.txt", "licenses/FluentSystemIcons-LICENSE.txt",
        "licenses/Hook-NOTICE.md", "licenses/ImGui-LICENSE.txt", "licenses/MinHook-LICENSE.txt", "licenses/Selawik-LICENSE.txt",
    };
    public const int MaxFiles = 128;
    public const long MaxPackageBytes = 1024L * 1024 * 1024;
    public const int MaxTransactions = 8;

    /// <summary>The exact canonical portable-runtime paths shared by package verification and installation.</summary>
    public static bool IsRuntimePath(string? path) => !string.IsNullOrWhiteSpace(path) && !path.Contains('\\') &&
        RuntimeNames.TryGetValue(path, out var canonical) && path == canonical && !path.Contains(':') && !Path.IsPathRooted(path);

    /// <summary>Explicit colocated game markers only; does not walk a game tree or inspect account/game configuration.</summary>
    public static bool IsGameDirectory(string root)
    {
        if (string.IsNullOrWhiteSpace(root)) return false;
        var full = Path.GetFullPath(root);
        return File.Exists(Path.Combine(full, "azimuth.exe")) ||
            File.Exists(Path.Combine(full, "forza_durango_release_final.exe")) ||
            File.Exists(Path.Combine(full, "AppxManifest.xml")) ||
            File.Exists(Path.Combine(full, "Mount", "forza_durango_release_final.exe")) ||
            File.Exists(Path.Combine(full, "Mount", "AppxManifest.xml"));
    }

    /// <summary>Creates the one destination-volume stage recognized by the installer.</summary>
    public static string CreateStage(string installRoot, string transactionId)
    {
        var transaction = TransactionRoot(installRoot, transactionId);
        using var exclusive = Lock(installRoot);
        PruneCore(installRoot, keep: 1);
        var store = Path.Combine(Root(installRoot), ".infinity-update");
        var transactions = TransactionDirectories(store);
        if (FindPendingRecovery(installRoot) is not null)
            throw new InvalidOperationException("Finish update recovery before staging another release.");
        if (!Directory.Exists(transaction) && transactions.Length >= MaxTransactions)
            throw new InvalidOperationException("Too many unfinished update stages; recovery is required.");
        var stage = Path.Combine(transaction, "stage");
        RequireNoReparse(stage);
        Directory.CreateDirectory(stage);
        RequireNoReparse(stage);
        return stage;
    }

    public static UpdateApplyPlan CreatePlan(string installRoot, string transactionId,
        IReadOnlyList<UpdateFile> files, IReadOnlyList<string> restartArguments)
    {
        using var parent = System.Diagnostics.Process.GetCurrentProcess();
        var root = Root(installRoot);
        return new(root, Path.Combine(TransactionRoot(root, transactionId), "stage"), transactionId, files, restartArguments)
        {
            ParentProcessId = parent.Id,
            ParentStartTimeUtcTicks = parent.StartTime.ToUniversalTime().Ticks,
            HealthNonce = Convert.ToHexStringLower(RandomNumberGenerator.GetBytes(32)),
        };
    }

    public static string WritePlan(UpdateApplyPlan plan)
    {
        ArgumentNullException.ThrowIfNull(plan);
        ValidatePlan(plan);
        var path = PlanPath(plan);
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        UpdateJournalStore.Write(path, plan);
        return path;
    }

    public static UpdateApplyPlan ReadPlan(string path)
    {
        var plan = UpdateJournalStore.Read<UpdateApplyPlan>(path);
        ValidatePlan(plan);
        if (!SamePath(path, PlanPath(plan))) throw new InvalidDataException("Update plan is outside its transaction.");
        return plan;
    }

    /// <summary>Call after catalogs and the replacement shell have initialized. Does not clear server/version policy.</summary>
    public static void AcknowledgeStartup(string planPath, string nonce)
    {
        var plan = ReadPlan(planPath);
        if (!StringComparer.Ordinal.Equals(nonce, plan.HealthNonce) ||
            !SamePath(Environment.ProcessPath ?? string.Empty, Path.Combine(plan.InstallRoot, "infinity.exe")))
            throw new InvalidDataException("Update startup identity does not match.");
        foreach (var file in plan.Files) VerifyFile(Within(plan.InstallRoot, file.Path), file);
        using var process = System.Diagnostics.Process.GetCurrentProcess();
        UpdateJournalStore.Write(Path.Combine(TransactionRoot(plan.InstallRoot, plan.TransactionId), "health.json"),
            new UpdateHealth(plan.TransactionId, nonce, process.Id, process.StartTime.ToUniversalTime().Ticks));
    }

    /// <summary>The replacement must keep Play disabled after its ACK until the worker durably commits.</summary>
    public static async Task WaitForCommitAsync(UpdateApplyPlan plan, TimeSpan timeout, CancellationToken cancellation = default)
    {
        ValidatePlan(plan);
        if (timeout <= TimeSpan.Zero || timeout > TimeSpan.FromSeconds(45)) throw new ArgumentOutOfRangeException(nameof(timeout));
        var journalPath = Path.Combine(TransactionRoot(plan.InstallRoot, plan.TransactionId), "journal.json");
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        bound.CancelAfter(timeout);
        try
        {
            while (true)
            {
                bound.Token.ThrowIfCancellationRequested();
                RequireNoReparse(journalPath);
                if (!File.Exists(journalPath)) throw new InvalidDataException("The update commit journal is missing.");
                UpdateJournal? journal = null;
                try { journal = UpdateJournalStore.Read<UpdateJournal>(journalPath); }
                catch (IOException error) when (error is not (FileNotFoundException or DirectoryNotFoundException))
                {
                    // A bounded sharing/rename race never enables Play or mutates recovery state.
                }
                if (journal is not null)
                {
                    ValidateJournal(journal, plan.TransactionId);
                    if (journal.Phase == "rolled_back") throw new UpdateInstallException("The update was rolled back.", true);
                    if (journal.Phase == "committed")
                    {
                        foreach (var file in plan.Files) VerifyFile(Within(plan.InstallRoot, file.Path), file);
                        return;
                    }
                }
                await Task.Delay(50, bound.Token).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException) when (!cancellation.IsCancellationRequested)
        { throw new TimeoutException("The update worker did not commit in time."); }
    }

    /// <summary>The helper holds this parent process handle, checking image and creation time to reject PID reuse.</summary>
    public static async Task WaitForParentAsync(UpdateApplyPlan plan, TimeSpan timeout, CancellationToken cancellation = default)
    {
        ValidatePlan(plan);
        if (timeout <= TimeSpan.Zero || timeout > TimeSpan.FromMinutes(2)) throw new ArgumentOutOfRangeException(nameof(timeout));
        System.Diagnostics.Process parent;
        try { parent = System.Diagnostics.Process.GetProcessById(plan.ParentProcessId); }
        catch (ArgumentException) { throw new InvalidDataException("Update parent exited before its identity was verified."); }
        using (parent)
        {
            if (parent.StartTime.ToUniversalTime().Ticks != plan.ParentStartTimeUtcTicks ||
                parent.MainModule?.FileName is not { } image || !SamePath(image, Path.Combine(plan.InstallRoot, "infinity.exe")))
                throw new InvalidDataException("Update parent identity does not match.");
            using var helper = System.Diagnostics.Process.GetCurrentProcess();
            UpdateJournalStore.Write(Path.Combine(TransactionRoot(plan.InstallRoot, plan.TransactionId), "parent-ready.json"),
                new UpdateParentReady(plan.TransactionId, plan.HealthNonce, helper.Id, helper.StartTime.ToUniversalTime().Ticks,
                    plan.ParentProcessId, plan.ParentStartTimeUtcTicks));
            using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
            bound.CancelAfter(timeout);
            await parent.WaitForExitAsync(bound.Token).ConfigureAwait(false);
        }
    }

    /// <summary>The launcher must await this handshake before it exits; a PID cannot be authenticated after it is gone.</summary>
    public static async Task WaitForHelperReadyAsync(UpdateApplyPlan plan, System.Diagnostics.Process helper,
        TimeSpan timeout, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(helper);
        ValidatePlan(plan);
        if (timeout <= TimeSpan.Zero || timeout > TimeSpan.FromMinutes(1)) throw new ArgumentOutOfRangeException(nameof(timeout));
        var started = helper.StartTime.ToUniversalTime().Ticks;
        var ready = Path.Combine(TransactionRoot(plan.InstallRoot, plan.TransactionId), "parent-ready.json");
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        bound.CancelAfter(timeout);
        while (!helper.HasExited)
        {
            bound.Token.ThrowIfCancellationRequested();
            if (File.Exists(ready) && UpdateJournalStore.Read<UpdateParentReady>(ready) ==
                new UpdateParentReady(plan.TransactionId, plan.HealthNonce, helper.Id, started,
                    plan.ParentProcessId, plan.ParentStartTimeUtcTicks)) return;
            await Task.Delay(50, bound.Token).ConfigureAwait(false);
        }
        throw new IOException("The update helper exited before accepting its parent.");
    }

    /// <summary>Replaces all managed runtime files as one recoverable transaction, never user/game state.</summary>
    public async Task ApplyAsync(UpdateApplyPlan plan,
        Func<UpdateApplyPlan, CancellationToken, Task<bool>> confirmStartup, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(plan);
        ArgumentNullException.ThrowIfNull(confirmStartup);
        ValidatePlan(plan);
        plan = plan with { Files = plan.Files.ToArray(), PreviousFiles = plan.PreviousFiles.ToArray(), RestartArguments = plan.RestartArguments.ToArray() };
        if (Environment.ProcessPath is { } running && SamePath(running, Path.Combine(plan.InstallRoot, "infinity.exe")))
            throw new InvalidOperationException("The running launcher must hand installation to the updater helper.");
        using var exclusive = Lock(plan.InstallRoot);
        if (supervisedGameActive()) throw new InvalidOperationException("An active game prevents update installation.");
        RecoverCore(plan.InstallRoot);
        var transaction = TransactionRoot(plan.InstallRoot, plan.TransactionId);
        var journalPath = Path.Combine(transaction, "journal.json");
        if (File.Exists(journalPath)) throw new InvalidOperationException("Use a new update transaction after recovery.");
        WritePlan(plan);
        await verifier.VerifyAsync(plan, cancellation).ConfigureAwait(false);
        cancellation.ThrowIfCancellationRequested();
        ValidateStage(plan);
        if (supervisedGameActive()) throw new InvalidOperationException("An active game prevents update installation.");

        var replacements = plan.Files.ToDictionary(f => f.Path, StringComparer.OrdinalIgnoreCase);
        var previous = plan.PreviousFiles.ToDictionary(f => f.Path, StringComparer.OrdinalIgnoreCase);
        foreach (var retired in previous.Values.Where(f => !replacements.ContainsKey(f.Path)))
        {
            var path = Within(plan.InstallRoot, retired.Path);
            if (File.Exists(path)) VerifyFile(path, retired); // never retire a user-modified replacement of a prior managed file
        }
        var paths = previous.Keys.Concat(replacements.Keys).Distinct(StringComparer.OrdinalIgnoreCase)
            .Order(StringComparer.OrdinalIgnoreCase).ToArray();
        var originals = paths.Select(path => Original(plan.InstallRoot, path, replacements.GetValueOrDefault(path))).ToArray();
        if (originals.Sum(file => file.Size) > MaxPackageBytes)
            throw new InvalidDataException("The previous runtime is too large to back up safely.");
        var journal = new UpdateJournal(plan.TransactionId, "preparing", originals);
        UpdateJournalStore.Write(journalPath, journal);
        progress?.Invoke(new(UpdateInstallPhase.BackingUp));
        foreach (var original in originals.Where(f => f.Existed))
        {
            cancellation.ThrowIfCancellationRequested();
            CopyVerified(Within(plan.InstallRoot, original.Path), Within(Path.Combine(transaction, "backup"), original.Path),
                new(original.Path, original.Size, original.Sha256!));
        }
        journal = journal with { Phase = "applying" };
        UpdateJournalStore.Write(journalPath, journal);
        var startupConfirmed = false;
        try
        {
            progress?.Invoke(new(UpdateInstallPhase.BackupReady));
            foreach (var original in originals)
            {
                cancellation.ThrowIfCancellationRequested();
                if (supervisedGameActive()) throw new InvalidOperationException("An active game prevents update installation.");
                var destination = Within(plan.InstallRoot, original.Path);
                RequireOriginal(destination, original);
                if (original.Replacement is { } replacement)
                {
                    var candidate = Within(Path.Combine(transaction, "incoming"), original.Path);
                    CopyVerified(Within(plan.StageRoot, original.Path), candidate, replacement);
                    RequireNoReparse(destination);
                    Directory.CreateDirectory(Path.GetDirectoryName(destination)!);
                    File.Move(candidate, destination, overwrite: true);
                }
                else if (File.Exists(destination)) File.Delete(destination);
                progress?.Invoke(new(UpdateInstallPhase.FileApplied, original.Path));
            }
            foreach (var file in plan.Files) VerifyFile(Within(plan.InstallRoot, file.Path), file);
            journal = journal with { Phase = "installed" };
            UpdateJournalStore.Write(journalPath, journal);
            progress?.Invoke(new(UpdateInstallPhase.AwaitingStartup));
            if (!await confirmStartup(plan, cancellation).ConfigureAwait(false)) throw new IOException("The replacement did not confirm startup.");
            startupConfirmed = true;
            journal = journal with { Phase = "committed" };
            UpdateJournalStore.Write(journalPath, journal);
            progress?.Invoke(new(UpdateInstallPhase.Committed));
        }
        catch (Exception error)
        {
            if (startupConfirmed) throw new UpdateInstallException("The replacement started but its commit needs recovery.", false, error);
            try { Rollback(plan.InstallRoot, journal); }
            catch (Exception rollback) when (rollback is IOException or InvalidDataException or UnauthorizedAccessException or InvalidOperationException)
            { throw new UpdateInstallException("Update recovery must complete before playing.", false, new AggregateException(error, rollback)); }
            throw new UpdateInstallException("Update failed; the previous runtime was restored.", true, error);
        }
    }

    /// <summary>Restores every uncommitted transaction from its complete backup before another apply.</summary>
    public static bool Recover(string installRoot)
    {
        var root = Root(installRoot, requireMarker: false);
        if (Environment.ProcessPath is { } running && SamePath(running, Path.Combine(root, "infinity.exe")))
            throw new InvalidOperationException("The running launcher must hand recovery to the updater helper.");
        using var exclusive = Lock(root);
        return RecoverCore(root);
    }

    /// <summary>Read-only startup inspection. The running launcher must ask a verified worker to recover, then exit.</summary>
    public static UpdateRecovery? FindPendingRecovery(string installRoot)
    {
        var root = Root(installRoot, requireMarker: false);
        var store = Path.Combine(root, ".infinity-update");
        RequireNoReparse(store);
        if (!Directory.Exists(store)) return null;
        foreach (var directory in TransactionDirectories(store).Order(StringComparer.Ordinal))
        {
            var id = Path.GetFileName(directory);
            if (!Guid.TryParseExact(id, "N", out _)) continue;
            RequireNoReparse(directory);
            var journalPath = Path.Combine(directory, "journal.json");
            if (!File.Exists(journalPath)) continue;
            var journal = UpdateJournalStore.Read<UpdateJournal>(journalPath);
            ValidateJournal(journal, id);
            if (journal.Phase is "committed" or "rolled_back") continue;
            return new(id, Path.Combine(directory, "plan.json"), Path.Combine(directory, "worker.exe"));
        }
        return null;
    }

    /// <summary>
    /// Deletes only validated terminal transactions. Keep the newest requested terminal history and
    /// always the newest committed backup; unknown contents or in-use files are left for explicit recovery.
    /// </summary>
    public static int PruneCompleted(string installRoot, int keep = 1)
    {
        if (keep is < 1 or > 2) throw new ArgumentOutOfRangeException(nameof(keep));
        var root = Root(installRoot);
        var store = Path.Combine(root, ".infinity-update");
        RequireNoReparse(store);
        if (!Directory.Exists(store)) return 0;
        using var exclusive = Lock(root);
        return PruneCore(root, keep);
    }

    /// <summary>Abandons only the caller's non-mutating stage. Unknown contents or active/committed journals are preserved.</summary>
    public static bool DiscardPreparedStage(string installRoot, string transactionId)
    {
        var root = Root(installRoot);
        var transaction = TransactionRoot(root, transactionId);
        if (!Directory.Exists(transaction)) return true;
        try
        {
            using var exclusive = Lock(root);
            var journalPath = Path.Combine(transaction, "journal.json");
            if (File.Exists(journalPath))
            {
                var journal = UpdateJournalStore.Read<UpdateJournal>(journalPath);
                ValidateJournal(journal, transactionId);
                if (journal.Phase is not ("preparing" or "rolled_back")) return false;
            }
            var planPath = Path.Combine(transaction, "plan.json");
            var plan = File.Exists(planPath) ? ReadPlan(planPath) : null;
            if (plan is not null && !SamePath(plan.InstallRoot, root)) return false;
            if (plan is not null && HelperMayBeRunning(transaction, plan)) return false;
            DeleteCheckedTransaction(transaction, plan);
            return true;
        }
        catch (Exception error) when (error is IOException or InvalidDataException or UnauthorizedAccessException or System.Text.Json.JsonException)
        { return false; }
    }

    /// <summary>
    /// Startup-only reclamation of crashed downloads: preserve an exact live parent, and preserve
    /// an unlabelled stage for at least an hour. Active apply journals always require recovery instead.
    /// </summary>
    public static int ReclaimAbandonedStages(string installRoot)
    {
        var root = Root(installRoot);
        var store = Path.Combine(root, ".infinity-update");
        RequireNoReparse(store);
        if (!Directory.Exists(store)) return 0;
        using var exclusive = Lock(root);
        var reclaimed = 0;
        foreach (var transaction in TransactionDirectories(store))
        {
            if (File.Exists(Path.Combine(transaction, "journal.json"))) continue;
            try
            {
                var planPath = Path.Combine(transaction, "plan.json");
                var plan = File.Exists(planPath) ? ReadPlan(planPath) : null;
                if (plan is not null)
                {
                    if (!SamePath(plan.InstallRoot, root) || !ParentIsGone(plan) || HelperMayBeRunning(transaction, plan)) continue;
                }
                else if (Directory.GetLastWriteTimeUtc(transaction) > DateTime.UtcNow - TimeSpan.FromHours(1)) continue;
                DeleteCheckedTransaction(transaction, plan);
                reclaimed++;
            }
            catch (Exception error) when (error is IOException or InvalidDataException or UnauthorizedAccessException or System.Text.Json.JsonException)
            {
                // Unknown/locked files are left for explicit recovery rather than guessed to be abandoned.
            }
        }
        return reclaimed;
    }

    private static bool ParentIsGone(UpdateApplyPlan plan)
        => ProcessIdentityIsGone(plan.ParentProcessId, plan.ParentStartTimeUtcTicks);

    private static bool HelperMayBeRunning(string transaction, UpdateApplyPlan plan)
    {
        var path = Path.Combine(transaction, "parent-ready.json");
        if (!File.Exists(path)) return false;
        var ready = UpdateJournalStore.Read<UpdateParentReady>(path);
        if (ready.TransactionId != plan.TransactionId || ready.Nonce != plan.HealthNonce || ready.ProcessId <= 0 || ready.StartTimeUtcTicks <= 0)
            throw new InvalidDataException("Unknown update helper identity prevents cleanup.");
        return !ProcessIdentityIsGone(ready.ProcessId, ready.StartTimeUtcTicks);
    }

    private static bool ProcessIdentityIsGone(int processId, long startTimeUtcTicks)
    {
        try
        {
            using var parent = System.Diagnostics.Process.GetProcessById(processId);
            return parent.HasExited || parent.StartTime.ToUniversalTime().Ticks != startTimeUtcTicks;
        }
        catch (ArgumentException) { return true; }
        catch (Exception error) when (error is InvalidOperationException or System.ComponentModel.Win32Exception or UnauthorizedAccessException)
        { return false; } // inability to inspect another live process is not evidence that it died
    }

    /// <summary>After rollback, start the old launcher normally so the mandatory update gate remains authoritative.</summary>
    public static void RestartRecovered(UpdateApplyPlan plan)
    {
        ValidateArguments(plan.RestartArguments);
        var executable = Path.Combine(Root(plan.InstallRoot), "infinity.exe");
        RequireNoReparse(executable);
        var start = new ProcessStartInfo(executable) { UseShellExecute = false, WorkingDirectory = plan.InstallRoot, CreateNoWindow = true };
        foreach (var argument in plan.RestartArguments) start.ArgumentList.Add(argument);
        using var child = System.Diagnostics.Process.Start(start) ?? throw new IOException("Could not start the restored launcher.");
    }

    /// <summary>Only a pre-mutation rejection may reopen the old launcher without a separate recovery handoff.</summary>
    public static bool CanRestartAfterPreflightFailure(UpdateApplyPlan plan, bool replacementMayBeRunning)
    {
        ValidatePlan(plan);
        if (replacementMayBeRunning || FindPendingRecovery(plan.InstallRoot) is not null) return false;
        var journalPath = Path.Combine(TransactionRoot(plan.InstallRoot, plan.TransactionId), "journal.json");
        if (!File.Exists(journalPath)) return true;
        var journal = UpdateJournalStore.Read<UpdateJournal>(journalPath);
        ValidateJournal(journal, plan.TransactionId);
        // A committed transaction may already have its replacement running; never start a second one.
        return journal.Phase == "rolled_back";
    }

    /// <summary>Starts only the installed launcher with safe configuration switches; failed startup is stopped before rollback.</summary>
    public static async Task<bool> RestartAndConfirmAsync(UpdateApplyPlan plan, CancellationToken cancellation = default)
    {
        ValidatePlan(plan);
        var transaction = TransactionRoot(plan.InstallRoot, plan.TransactionId);
        var healthPath = Path.Combine(transaction, "health.json");
        RequireNoReparse(healthPath);
        if (File.Exists(healthPath)) File.Delete(healthPath);
        var start = new ProcessStartInfo(Path.Combine(plan.InstallRoot, "infinity.exe"))
        { UseShellExecute = false, WorkingDirectory = plan.InstallRoot, CreateNoWindow = true };
        foreach (var argument in plan.RestartArguments) start.ArgumentList.Add(argument);
        start.ArgumentList.Add("--updated-transaction");
        start.ArgumentList.Add(PlanPath(plan));
        start.ArgumentList.Add("--updated-nonce");
        start.ArgumentList.Add(plan.HealthNonce);
        using var child = System.Diagnostics.Process.Start(start) ?? throw new IOException("Could not start the replacement.");
        var started = child.StartTime.ToUniversalTime().Ticks;
        var healthy = false;
        try
        {
            using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
            bound.CancelAfter(TimeSpan.FromSeconds(30));
            while (!child.HasExited)
            {
                bound.Token.ThrowIfCancellationRequested();
                if (File.Exists(healthPath))
                {
                    var health = UpdateJournalStore.Read<UpdateHealth>(healthPath);
                    if (health == new UpdateHealth(plan.TransactionId, plan.HealthNonce, child.Id, started))
                    { healthy = true; return true; }
                }
                await Task.Delay(100, bound.Token).ConfigureAwait(false);
            }
            return false;
        }
        catch (OperationCanceledException) when (!cancellation.IsCancellationRequested) { return false; }
        finally
        {
            if (!healthy && !child.HasExited)
            {
                child.Kill();
                await child.WaitForExitAsync(CancellationToken.None).WaitAsync(TimeSpan.FromSeconds(5), CancellationToken.None).ConfigureAwait(false);
            }
        }
    }

    /// <summary>The launcher's installation folder, also for the helper running from a transaction inside it.</summary>
    private static string InstallBoundary
    {
        get
        {
            var base_ = Path.TrimEndingDirectorySeparator(Path.GetFullPath(AppContext.BaseDirectory));
            var store = base_.IndexOf(Path.DirectorySeparatorChar + ".infinity-update", StringComparison.OrdinalIgnoreCase);
            return store > 0 ? base_[..store] : base_;
        }
    }

    private static bool Above(string item, string boundary)
    {
        var parent = Path.EndsInDirectorySeparator(item) ? item : item + Path.DirectorySeparatorChar;
        return boundary.StartsWith(parent, StringComparison.OrdinalIgnoreCase);
    }

    internal static void RequireNoReparse(string path)
    {
        for (var item = Path.GetFullPath(path); !string.IsNullOrEmpty(item); item = Path.GetDirectoryName(item))
        {
            // Under Wine every drive root and Linux mount point reports a reparse point, with no tag or target to tell
            // it from a link: there, as in ContentPath.SafeWithin, only the installation and what lies inside it count.
            if (Infinity.Core.Process.WineHost.Detected && Above(item, InstallBoundary)) continue;
            try
            {
                if ((File.GetAttributes(item) & FileAttributes.ReparsePoint) != 0)
                    throw new InvalidDataException("Update paths cannot contain links or reparse points.");
            }
            catch (Exception error) when (error is FileNotFoundException or DirectoryNotFoundException)
            {
                // Missing descendants are allowed; still inspect every existing ancestor (including dangling links).
            }
        }
    }

    private static bool RecoverCore(string root)
    {
        var store = Path.Combine(root, ".infinity-update");
        RequireNoReparse(store);
        if (!Directory.Exists(store)) return false;
        var recovered = false;
        foreach (var directory in TransactionDirectories(store))
        {
            var id = Path.GetFileName(directory);
            if (!Guid.TryParseExact(id, "N", out _)) continue;
            RequireNoReparse(directory);
            var path = Path.Combine(directory, "journal.json");
            if (!File.Exists(path)) continue;
            var journal = UpdateJournalStore.Read<UpdateJournal>(path);
            ValidateJournal(journal, id);
            if (journal.Phase is "committed" or "rolled_back") continue;
            if (journal.Phase == "preparing")
                UpdateJournalStore.Write(path, journal with { Phase = "rolled_back" });
            else Rollback(root, journal);
            recovered = true;
        }
        return recovered;
    }

    private static string[] TransactionDirectories(string store)
    {
        RequireNoReparse(store);
        var entries = Directory.EnumerateFileSystemEntries(store).Take(MaxTransactions + 2).ToArray();
        if (entries.Length > MaxTransactions + 1) throw new InvalidDataException("Update storage has too many unresolved entries.");
        var directories = new List<string>();
        foreach (var entry in entries)
        {
            RequireNoReparse(entry);
            if (Path.GetFileName(entry) == "install.lock" && File.Exists(entry)) continue;
            if (!Directory.Exists(entry) || !Guid.TryParseExact(Path.GetFileName(entry), "N", out _))
                throw new InvalidDataException("Unexpected update storage entry; explicit recovery is required.");
            directories.Add(entry);
        }
        return directories.ToArray();
    }

    private static int PruneCore(string root, int keep)
    {
        var store = Path.Combine(Root(root), ".infinity-update");
        RequireNoReparse(store);
        if (!Directory.Exists(store)) return 0;
        var terminal = new List<(string Directory, UpdateJournal Journal, DateTime Written)>();
        foreach (var directory in TransactionDirectories(store))
        {
            var journalPath = Path.Combine(directory, "journal.json");
            if (!File.Exists(journalPath)) continue;
            var journal = UpdateJournalStore.Read<UpdateJournal>(journalPath);
            ValidateJournal(journal, Path.GetFileName(directory));
            if (journal.Phase is "committed" or "rolled_back") terminal.Add((directory, journal, File.GetLastWriteTimeUtc(journalPath)));
        }
        var ordered = terminal.OrderByDescending(t => t.Written).ThenBy(t => t.Directory, StringComparer.Ordinal).ToArray();
        var preserve = ordered.Take(keep).Select(t => t.Directory).ToHashSet(StringComparer.OrdinalIgnoreCase);
        var committed = ordered.FirstOrDefault(t => t.Journal.Phase == "committed");
        if (committed.Directory is not null) preserve.Add(committed.Directory);
        var removed = 0;
        foreach (var item in ordered.Where(t => !preserve.Contains(t.Directory)))
        {
            try
            {
                var planPath = Path.Combine(item.Directory, "plan.json");
                var plan = ReadPlan(planPath);
                if (!SamePath(plan.InstallRoot, root)) throw new InvalidDataException("Update cleanup installation does not match.");
                if (HelperMayBeRunning(item.Directory, plan)) continue;
                DeleteCheckedTransaction(item.Directory, plan);
                removed++;
            }
            catch (Exception error) when (error is IOException or InvalidDataException or UnauthorizedAccessException or System.Text.Json.JsonException)
            {
                // No recursive delete or traversal of unrecognized contents. A locked worker may still be exiting.
            }
        }
        return removed;
    }

    private static void DeleteCheckedTransaction(string transaction, UpdateApplyPlan? plan)
    {
        var (files, directories) = CleanupInventory(transaction, plan);
        var handles = new List<FileStream>();
        try
        {
            // Check the entire bounded tree for locks before deleting any file.
            foreach (var file in files) handles.Add(new FileStream(file, FileMode.Open, FileAccess.Read, FileShare.Delete));
            foreach (var file in files.Where(p => !SamePath(Path.GetDirectoryName(p)!, transaction) || Path.GetFileName(p) == "worker.exe")
                         .OrderBy(p => Path.GetFileName(p) == "worker.exe" ? 0 : 1))
            {
                RequireNoReparse(file);
                File.Delete(file);
            }
        }
        finally { foreach (var handle in handles) handle.Dispose(); }
        foreach (var directory in directories.OrderByDescending(p => p.Length))
        {
            RequireNoReparse(directory);
            Directory.Delete(directory); // never recursive; an unexpected late entry stops cleanup
        }
        foreach (var file in files.Where(p => SamePath(Path.GetDirectoryName(p)!, transaction) && Path.GetFileName(p) != "worker.exe")
                     .OrderBy(p => Path.GetFileName(p) is "plan.json" or "journal.json" ? 1 : 0))
        {
            RequireNoReparse(file);
            File.Delete(file);
        }
        RequireNoReparse(transaction);
        Directory.Delete(transaction);
    }

    private static (string[] Files, string[] Directories) CleanupInventory(string transaction, UpdateApplyPlan? plan)
    {
        var metadata = new HashSet<string>(StringComparer.Ordinal)
        {
            "plan.json", "journal.json", "health.json", "parent-ready.json", "worker.exe", "worker.exe.sig", "release.json", "release.json.sig", "package.zip",
            "previous-release.json", "previous-release.json.sig", "previous-checksums.txt", "previous-checksums.txt.sig",
        };
        foreach (var descriptor in new[] { plan?.DescriptorPath, plan?.DescriptorSignaturePath,
                     plan?.PreviousDescriptorPath, plan?.PreviousDescriptorSignaturePath,
                     plan?.PreviousChecksumsPath, plan?.PreviousChecksumsSignaturePath }.OfType<string>()) metadata.Add(Path.GetFileName(descriptor));
        foreach (var name in metadata.ToArray()) metadata.Add(name + ".tmp");
        var files = new List<string>();
        var directories = new List<string>();
        var pending = new Stack<string>();
        pending.Push(transaction);
        while (pending.TryPop(out var directory))
        {
            RequireNoReparse(directory);
            foreach (var entry in Directory.EnumerateFileSystemEntries(directory).Take(MaxFiles * 6 + 1))
            {
                RequireNoReparse(entry);
                var relative = Path.GetRelativePath(transaction, entry).Replace('\\', '/');
                if (Directory.Exists(entry))
                {
                    var split = relative.Split('/');
                    if (split[0] is not ("stage" or "backup" or "incoming" or "restore") || split.Length > 2 ||
                        (split.Length == 2 && !RuntimeDirectories.Contains(split[1], StringComparer.Ordinal)))
                        throw new InvalidDataException("Unexpected update transaction directory.");
                    directories.Add(entry);
                    if (directories.Count > 24) throw new InvalidDataException("Update cleanup tree is too large.");
                    pending.Push(entry);
                }
                else
                {
                    if (relative.Contains('/'))
                    {
                        var separator = relative.IndexOf('/');
                        if (!IsRuntimePath(relative[(separator + 1)..])) throw new InvalidDataException("Unexpected update transaction file.");
                    }
                    else if (!metadata.Contains(relative)) throw new InvalidDataException("Unknown update metadata prevents cleanup.");
                    files.Add(entry);
                    if (files.Count > MaxFiles * 6) throw new InvalidDataException("Update cleanup inventory is too large.");
                }
            }
        }
        return (files.ToArray(), directories.ToArray());
    }

    private static void Rollback(string root, UpdateJournal journal)
    {
        ValidateJournal(journal, journal.TransactionId);
        var transaction = TransactionRoot(root, journal.TransactionId);
        foreach (var original in journal.Files)
        {
            var current = Within(root, original.Path);
            var previous = original.Existed ? new UpdateFile(original.Path, original.Size, original.Sha256!) : null;
            if (original.Existed && !Matches(current, previous))
                VerifyFile(Within(Path.Combine(transaction, "backup"), original.Path), previous!);
            if (File.Exists(current) && !Matches(current, previous)
                && !Matches(current, original.Replacement))
                throw new InvalidDataException("A runtime file changed outside the update transaction.");
        }
        foreach (var original in journal.Files)
        {
            var destination = Within(root, original.Path);
            if (original.Existed)
            {
                var previous = new UpdateFile(original.Path, original.Size, original.Sha256!);
                if (Matches(destination, previous)) continue; // already restored before an interruption
                var backup = Within(Path.Combine(transaction, "backup"), original.Path);
                VerifyFile(backup, previous);
                RequireNoReparse(destination);
                Directory.CreateDirectory(Path.GetDirectoryName(destination)!);
                // Same-volume rename needs no extra package-sized allocation when failure was disk-full.
                // A crash after this rename is recognizable from the original digest at destination.
                File.Move(backup, destination, overwrite: true);
            }
            else if (File.Exists(destination)) File.Delete(destination);
        }
        UpdateJournalStore.Write(Path.Combine(transaction, "journal.json"), journal with { Phase = "rolled_back" });
    }

    private static void ValidatePlan(UpdateApplyPlan plan)
    {
        var root = Root(plan.InstallRoot);
        var transaction = TransactionRoot(root, plan.TransactionId);
        if (!SamePath(plan.StageRoot, Path.Combine(transaction, "stage"))) throw new InvalidDataException("Invalid update stage location.");
        if (plan.Files is null || plan.Files.Count is < 1 or > MaxFiles || plan.RestartArguments is null ||
            plan.PreviousFiles is null || plan.PreviousFiles.Count > MaxFiles)
            throw new InvalidDataException("Invalid update inventory.");
        var names = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        long total = 0;
        foreach (var file in plan.Files)
        {
            ValidateFile(file);
            if (!names.Add(file.Path)) throw new InvalidDataException("Duplicate update file.");
            total = checked(total + file.Size);
        }
        if (!names.Contains("infinity.exe") || total > MaxPackageBytes) throw new InvalidDataException("Invalid update runtime package.");
        names.Clear();
        long previousTotal = 0;
        foreach (var file in plan.PreviousFiles)
        {
            ValidateFile(file);
            if (!names.Add(file.Path)) throw new InvalidDataException("Duplicate previous update file.");
            previousTotal = checked(previousTotal + file.Size);
        }
        if (previousTotal > MaxPackageBytes) throw new InvalidDataException("Previous update inventory is too large.");
        if (plan.ParentProcessId <= 0 || plan.ParentStartTimeUtcTicks <= 0 || !Digest(plan.HealthNonce))
            throw new InvalidDataException("Invalid update process identity.");
        foreach (var descriptor in new[] { plan.DescriptorPath, plan.DescriptorSignaturePath,
                     plan.PreviousDescriptorPath, plan.PreviousDescriptorSignaturePath,
                     plan.PreviousChecksumsPath, plan.PreviousChecksumsSignaturePath }.OfType<string>())
        {
            if (!SamePath(Path.GetDirectoryName(Path.GetFullPath(descriptor))!, transaction))
                throw new InvalidDataException("Update descriptor is outside its transaction.");
            RequireNoReparse(descriptor);
        }
        ValidateArguments(plan.RestartArguments);
        RequireNoReparse(plan.StageRoot);
    }

    private static void ValidateArguments(IReadOnlyList<string> arguments)
    {
        if (arguments.Count > 8 || arguments.Count % 2 != 0) throw new InvalidDataException("Invalid update restart arguments.");
        var names = new HashSet<string>(StringComparer.Ordinal);
        for (var index = 0; index < arguments.Count; index += 2)
        {
            var name = arguments[index];
            var value = arguments[index + 1];
            if (name is null || value is null || !names.Add(name) || value.Length > 2048) throw new InvalidDataException("Invalid update restart arguments.");
            if (name == "--locale")
            { if (value is not ("en" or "pt-BR")) throw new InvalidDataException("Invalid update locale."); }
            else if (name == "--environment")
            { if (value is not ("development" or "production")) throw new InvalidDataException("Invalid update environment."); }
            else if (name is "--server" or "--portal")
            {
                if (!Uri.TryCreate(value, UriKind.Absolute, out var origin) || origin.UserInfo.Length != 0 ||
                    origin.Query.Length != 0 || origin.Fragment.Length != 0)
                    throw new InvalidDataException("Invalid update restart origin.");
                ApiClient.CheckOrigin(origin);
            }
            else throw new InvalidDataException("Only environment, server, portal and locale may be preserved across update.");
        }
        if (names.Contains("--environment") || BuildProfile.ProductionOnly)
            Auth.ServerConfig.From(arguments, _ => null);
    }

    private static void ValidateJournal(UpdateJournal journal, string expectedId)
    {
        if (journal.TransactionId != expectedId || !Guid.TryParseExact(journal.TransactionId, "N", out _) ||
            journal.Phase is not ("preparing" or "applying" or "installed" or "committed" or "rolled_back") ||
            journal.Files is null || journal.Files.Count is < 1 or > MaxFiles)
            throw new InvalidDataException("Invalid update recovery journal.");
        var names = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        foreach (var entry in journal.Files)
        {
            if (entry is null) throw new InvalidDataException("Invalid null update recovery file.");
            ValidateName(entry.Path);
            if (!names.Add(entry.Path) || entry.Size is < 0 or > MaxPackageBytes ||
                (entry.Existed && !Digest(entry.Sha256)) || (!entry.Existed && (entry.Size != 0 || entry.Sha256 is not null)))
                throw new InvalidDataException("Invalid update recovery file.");
            if (entry.Replacement is { } replacement)
            {
                ValidateFile(replacement);
                if (entry.Path != replacement.Path) throw new InvalidDataException("Invalid update replacement path.");
            }
        }
    }

    private static void ValidateStage(UpdateApplyPlan plan)
    {
        var expected = plan.Files.Select(f => f.Path).ToHashSet(StringComparer.OrdinalIgnoreCase);
        var actual = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        var pending = new Stack<string>();
        pending.Push(plan.StageRoot);
        while (pending.TryPop(out var directory))
        {
            RequireNoReparse(directory);
            foreach (var path in Directory.EnumerateFileSystemEntries(directory))
            {
                RequireNoReparse(path);
                if (Directory.Exists(path))
                {
                    var relative = Path.GetRelativePath(plan.StageRoot, path).Replace('\\', '/');
                    if (!RuntimeDirectories.Contains(relative, StringComparer.OrdinalIgnoreCase)) throw new InvalidDataException("Unexpected update directory.");
                    pending.Push(path);
                }
                else
                {
                    var relative = Path.GetRelativePath(plan.StageRoot, path).Replace('\\', '/');
                    if (!actual.Add(relative) || !expected.Contains(relative)) throw new InvalidDataException("Unexpected update file.");
                }
            }
        }
        if (!actual.SetEquals(expected)) throw new InvalidDataException("Incomplete update runtime package.");
        foreach (var file in plan.Files) VerifyFile(Within(plan.StageRoot, file.Path), file);
    }

    private static UpdateOriginal Original(string root, string path, UpdateFile? replacement)
    {
        var absolute = Within(root, path);
        if (Directory.Exists(absolute)) throw new InvalidDataException("An update file destination is a directory.");
        if (!File.Exists(absolute)) return new(path, false, 0, null, replacement);
        using var stream = File.OpenRead(absolute);
        if (stream.Length > MaxPackageBytes) throw new InvalidDataException("Installed runtime file is too large.");
        return new(path, true, stream.Length, Convert.ToHexStringLower(SHA256.HashData(stream)), replacement);
    }

    private static void RequireOriginal(string path, UpdateOriginal original)
    {
        RequireNoReparse(path);
        if (original.Existed)
            VerifyFile(path, new(original.Path, original.Size, original.Sha256!));
        else if (File.Exists(path) || Directory.Exists(path)) throw new IOException("An update destination appeared unexpectedly.");
    }

    private static void CopyVerified(string source, string destination, UpdateFile file)
    {
        RequireNoReparse(source);
        RequireNoReparse(destination);
        Directory.CreateDirectory(Path.GetDirectoryName(destination)!);
        using (var input = new FileStream(source, FileMode.Open, FileAccess.Read, FileShare.Read))
        using (var output = new FileStream(destination, FileMode.CreateNew, FileAccess.Write, FileShare.None, 81920, FileOptions.WriteThrough))
        {
            if (input.Length != file.Size) throw new InvalidDataException("Update file size changed.");
            input.CopyTo(output);
            output.Flush(flushToDisk: true);
        }
        VerifyFile(destination, file);
    }

    private static bool Matches(string path, UpdateFile? file)
    {
        if (file is null) return false;
        try { VerifyFile(path, file); return true; }
        catch (Exception error) when (error is IOException or InvalidDataException) { return false; }
    }

    private static void VerifyFile(string path, UpdateFile file)
    {
        RequireNoReparse(path);
        using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.Read);
        if (stream.Length != file.Size || !StringComparer.Ordinal.Equals(Convert.ToHexStringLower(SHA256.HashData(stream)), file.Sha256))
            throw new InvalidDataException("Update file hash or size does not match.");
    }

    private static void ValidateFile(UpdateFile file)
    {
        if (file is null) throw new InvalidDataException("Invalid null update file.");
        ValidateName(file.Path);
        if (file.Size is < 0 or > MaxPackageBytes || !Digest(file.Sha256)) throw new InvalidDataException("Invalid update file metadata.");
    }

    private static void ValidateName(string path)
    {
        if (!IsRuntimePath(path)) throw new InvalidDataException("Only launcher runtime paths may be updated.");
    }

    private static bool Digest(string? text) => text is { Length: 64 } && text.All(c => c is >= '0' and <= '9' or >= 'a' and <= 'f');
    private static bool SamePath(string a, string b) => !string.IsNullOrWhiteSpace(a) && !string.IsNullOrWhiteSpace(b) &&
        StringComparer.OrdinalIgnoreCase.Equals(Path.GetFullPath(a), Path.GetFullPath(b));

    private static string Root(string root, bool requireMarker = true)
    {
        if (string.IsNullOrWhiteSpace(root) || !Path.IsPathFullyQualified(root)) throw new InvalidDataException("Update installation path must be absolute.");
        var full = Path.TrimEndingDirectorySeparator(Path.GetFullPath(root));
        if (SamePath(full, Path.GetPathRoot(full)!)) throw new InvalidDataException("A volume root is not a launcher installation.");
        RequireNoReparse(full);
        if (IsGameDirectory(full)) throw new InvalidDataException("Move Infinity to its own writable folder before updating.");
        if (!Directory.Exists(full) || (requireMarker && !File.Exists(Path.Combine(full, "infinity.exe"))))
            throw new InvalidDataException("The launcher installation marker is missing.");
        return full;
    }

    private static string TransactionRoot(string root, string transactionId)
    {
        if (!Guid.TryParseExact(transactionId, "N", out _)) throw new InvalidDataException("Invalid update transaction id.");
        var path = Path.Combine(Root(root, requireMarker: false), ".infinity-update", transactionId);
        RequireNoReparse(path);
        return path;
    }

    private static string Within(string root, string relative)
    {
        ValidateName(relative);
        var path = Path.GetFullPath(Path.Combine(root, relative.Replace('/', Path.DirectorySeparatorChar)));
        var prefix = Path.TrimEndingDirectorySeparator(Path.GetFullPath(root)) + Path.DirectorySeparatorChar;
        if (!path.StartsWith(prefix, StringComparison.OrdinalIgnoreCase)) throw new InvalidDataException("Update path escapes its root.");
        RequireNoReparse(path);
        return path;
    }

    private static string PlanPath(UpdateApplyPlan plan) => Path.Combine(TransactionRoot(plan.InstallRoot, plan.TransactionId), "plan.json");

    private static FileStream Lock(string root)
    {
        var directory = Path.Combine(Root(root, requireMarker: false), ".infinity-update");
        RequireNoReparse(directory);
        Directory.CreateDirectory(directory);
        var path = Path.Combine(directory, "install.lock");
        RequireNoReparse(path);
        return new FileStream(path, FileMode.OpenOrCreate, FileAccess.ReadWrite, FileShare.None);
    }
}
