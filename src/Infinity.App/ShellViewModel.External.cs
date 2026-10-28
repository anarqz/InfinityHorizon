// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Globalization;
using System.IO;
using System.Runtime.CompilerServices;
using Infinity.Core.Mods;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>One source row of the set-up pane (OD-117): what to choose, where it was found, and how its files checked.</summary>
public sealed class ExternalSourceRow(ExternalSource source, string label, string help, string kind) : INotifyPropertyChanged
{
    public ExternalSource Source { get; } = source;
    public string Label { get; } = label;
    public string Help { get; } = help;
    public bool PicksFolder => kind is "folder" or "either";
    public bool PicksFile => kind is "file" or "either";
    private string _status = ""; private string _details = ""; private bool _ok; private bool _checking; private double _progress; private bool _problems;
    public string Status { get => _status; set => Set(ref _status, value); }
    /// <summary>The per-file list behind a problem summary (hashes only here, never first).</summary>
    public string Details { get => _details; set { Set(ref _details, value); Changed(nameof(HasDetails)); } }
    public bool HasDetails => _details.Length > 0;
    public bool Ok { get => _ok; set { Set(ref _ok, value); Changed(nameof(ShowCheckMark)); } }
    public bool Problems { get => _problems; set { Set(ref _problems, value); Changed(nameof(ShowWarningMark)); } }
    private bool _notice;
    /// <summary>Usable, but something in the pick needs the player's eye (another version of a file the store already holds).</summary>
    public bool Notice { get => _notice; set { Set(ref _notice, value); Changed(nameof(ShowCheckMark)); Changed(nameof(ShowWarningMark)); } }
    public bool ShowCheckMark => _ok && !_notice;
    public bool ShowWarningMark => _problems || _notice;
    public bool Checking { get => _checking; set => Set(ref _checking, value); }
    public double Progress { get => _progress; set => Set(ref _progress, value); }
    public string? Picked { get; set; }

    public event PropertyChangedEventHandler? PropertyChanged;
    private void Set<T>(ref T field, T value, [CallerMemberName] string? name = null) { field = value; Changed(name); }
    private void Changed(string? name) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}

/// <summary>An entry of the in-launcher browser: a folder, or a file the source accepts (a ZIP, or any file for a file source).</summary>
public sealed record BrowserEntry(string Name, string FullPath, bool Folder);

/// <summary>A quick location of the browser (Home, Downloads, SD card, a drive…), shown only when it exists.</summary>
public sealed record BrowserPlace(string Label, string Path);

/// <summary>
/// OD-117: the set-up pane of a mod made of the player's own files. One row per source; the player picks a folder (the pack or
/// any parent), a file or a ZIP with the launcher's own picker (<see cref="OpenPicker"/>). Each pick is checked with
/// progress; Install is enabled only when every source verifies, after the short "your own copy" notice.
/// </summary>
public sealed partial class ShellViewModel
{
    private ModManifest? _externalMod;
    private readonly Dictionary<ExternalSourceRow, CancellationTokenSource> _externalChecks = [];
    public ObservableCollection<ExternalSourceRow> ExternalRows { get; } = [];

    public bool ShowExternalSetup => _externalMod is not null && ShowMods;
    public string ExternalSetupTitle => _externalMod is null ? "" : S.Format("external.title", ("mod", _externalMod.Text("name", S.Locale)));
    public string ExternalNotice => _externalMod is { ExternalSources: [var first, ..] }
        ? S.Format("external.notice", ("hint", first.Hint ?? first.LabelText(S.Locale))) : "";
    public bool ExternalAllOk => ExternalRows.Count > 0 && ExternalRows.All(r => r.Ok);
    public bool CanInstallExternal => ModsIdle && ExternalAllOk && !ExternalRows.Any(r => r.Checking) && _externalMod is not null && _install is not null && !ModRecoveryRequired
        && !ExternalNeedsGameFolder;
    /// <summary>The mod writes game files and no game folder is chosen: the pane asks for it before Install (never a refusal after it).</summary>
    public bool ExternalNeedsGameFolder => _externalMod is { } mod && ContentLocation is null && mod.Files.Any(f => f.Root == "game");
    public bool ExternalInstalled => _externalMod is { } mod && _modHealth.Installed.Any(i => i.Id == mod.Id);
    public string ExternalInstallLabel => ExternalInstalled && !ExternalUpdate ? S["external.recheck_done"]
        : ExternalReplaced.Count > 0 ? Counted("external.install_replacing", ExternalReplaced.Count) : S["external.install"];
    private bool ExternalUpdate => _externalMod is { } mod && _modHealth.Installed.FirstOrDefault(i => i.Id == mod.Id)?.Manifest?.SameContent(mod) == false;

    /// <summary>
    /// Before Install: the game folder the files go to and the existing files they replace (backed up, restored on disable or
    /// uninstall). Shown so a wrong folder — a real game picked for a test pack — is visible before anything is written.
    /// </summary>
    private IReadOnlyList<string> ExternalReplaced
    {
        get
        {
            // Azimuth-root files count without a game folder too (review, T2 M7); game files need the folder.
            return _externalMod is { } mod ? ReplacedBy(mod) : [];
        }
    }
    /// <summary>The same install preview as every mod (Alex, 1.0.11); empty while a game folder it needs is missing (the pane says so).</summary>
    public string ExternalTargetText => _externalMod is { } mod && !(ContentLocation is null && mod.Files.Any(f => f.Root == "game")) ? PreviewTextFor(mod) : "";
    public bool ExternalReplaces => ExternalAllOk && ExternalReplaced.Count > 0;
    public string ExternalReplacesText => ExternalReplaced is { Count: > 0 } files
        ? Counted("external.replaces", files.Count, ("files", FileList(files))) : "";

    /// <summary>
    /// Opens the pane for <paramref name="card"/>'s mod (<paramref name="update"/>: its reviewed pending update, from the update's own
    /// confirmation only): rows already copied show so; a remembered pick is checked again. "Check files again" never applies an update.
    /// </summary>
    public async Task OpenExternalSetupAsync(ModCard card, bool update = false)
    {
        ArgumentNullException.ThrowIfNull(card);
        var mod = update ? card.Next : card.Manifest;
        if (mod is not { IsExternal: true } || ModManagerFactory() is not { } factory || !ModsIdle || update && !card.CanAct) return;
        _externalMod = mod; _showModInfo = false; _showModOptions = false; _showLobbyPane = false; _selectedMod = card;
        if (ShowBrowser) StopBrowsing();
        ExternalRows.Clear();
        foreach (var source in mod.ExternalSources)
            ExternalRows.Add(new(source, source.LabelText(S.Locale), source.HelpText(S.Locale), source.Kind) { Status = S["external.not_chosen"] });
        Changed(string.Empty);
        IReadOnlyList<ExternalFile> missing; IReadOnlyDictionary<string, string> remembered;
        try
        {
            var manager = await Task.Run(factory);
            missing = await Task.Run(() => manager.ExternalFilesNotStored(mod));
            remembered = manager.ExternalChoices(mod.Id);
        }
        catch (Exception error) when (ManagedModFiles.IsStateError(error)) { Message = ModErrorText(error); return; }
        foreach (var row in ExternalRows)
        {
            if (missing.All(f => f.SourceId != row.Source.Id))
            {
                var count = ManagedModFiles.ExternalFiles(mod).Count(f => f.SourceId == row.Source.Id);
                row.Ok = true; row.Status = Counted("external.already", count);
            }
            else if (remembered.TryGetValue(row.Source.Id, out var path) && (Directory.Exists(path) || File.Exists(path))) await CheckExternalAsync(row, path);
        }
        Changed(string.Empty);
    }

    public void CloseExternalSetup()
    {
        foreach (var check in _externalChecks.Values) check.Cancel();
        _externalMod = null; _previewReplaced = null;
        if (ShowBrowser) StopBrowsing();
        ExternalRows.Clear(); BrowserEntries.Clear();
        Changed(string.Empty);
    }

    /// <summary>Checks one pick off the UI thread, with progress, copying every verified file into the store (OD-117).</summary>
    public async Task CheckExternalAsync(ExternalSourceRow row, string picked)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (_externalMod is not { } mod || ModManagerFactory() is not { } factory) return;
        if (_externalChecks.Remove(row, out var earlier)) earlier.Cancel(); // a new pick for the same row replaces its check; other rows go on
        using var cancel = new CancellationTokenSource(); _externalChecks[row] = cancel;
        row.Picked = picked; row.Checking = true; row.Ok = false; row.Problems = false; row.Details = ""; row.Progress = 0;
        row.Status = S["external.checking_start"]; Changed(nameof(CanInstallExternal));
        var progress = new Progress<ExternalProgress>(p =>
        {
            row.Progress = p.TotalBytes == 0 ? 1 : (double)p.Bytes / p.TotalBytes;
            row.Status = S.Format("external.checking", ("done", p.Files.ToString(CultureInfo.CurrentCulture)), ("total", p.TotalFiles.ToString(CultureInfo.CurrentCulture)),
                ("mb", (p.Bytes / (1024 * 1024)).ToString(CultureInfo.CurrentCulture)), ("total_mb", (p.TotalBytes / (1024 * 1024)).ToString(CultureInfo.CurrentCulture)));
        });
        try
        {
            var check = await Task.Run(() =>
            {
                var manager = factory();
                return manager.CheckExternalSource(mod, row.Source.Id, picked, progress, cancel.Token); // remembers a useful pick itself
            }, cancel.Token);
            ShowCheck(row, check);
        }
        catch (OperationCanceledException) { row.Status = S["external.not_chosen"]; }
        catch (Exception error) when (ManagedModFiles.IsStateError(error))
        {
            Log("mods: external source check failed: " + error.GetType().Name + (error is ModException code ? " " + code.Code : ""));
            row.Problems = true;
            row.Status = error is ModException { Code: ModErrors.ExternalFiles } failure ? S.Format("external.unusable", ("reason", S["external.reason." + failure.Message] is var reason && reason != "external.reason." + failure.Message ? reason : S["external.reason.other"])) : ModErrorText(error);
        }
        finally { row.Checking = false; if (_externalChecks.TryGetValue(row, out var current) && ReferenceEquals(current, cancel)) _externalChecks.Remove(row); Changed(nameof(CanInstallExternal)); Changed(nameof(ExternalAllOk)); }
    }

    private void ShowCheck(ExternalSourceRow row, ExternalSourceCheck check)
    {
        row.Ok = check.Ok; row.Problems = !check.Ok; row.Progress = 1; row.Notice = false; row.Details = "";
        if (check.Ok && check.DifferentInPick is { Count: > 0 } other)
        {
            // T2b: the install uses the stored, verified bytes; the pick's other version is named, never a plain check mark.
            row.Notice = true;
            row.Status = Counted("external.different_kept", other.Count)
                + "\n" + Counted("external.found", check.FromSource, ("where", check.MatchedRoot ?? check.Picked))
                + (check.Found - check.FromSource - other.Count is var kept and > 0 ? "\n" + Counted("external.kept", kept) : "");
            row.Details = string.Join("\n", other.Take(200).Select(f => S["external.file_different"] + "  " + f.Path
                + "\n    " + S.Format("external.file_expected", ("bytes", f.Bytes.ToString("N0", CultureInfo.CurrentCulture)), ("sha", f.Sha256))));
            return;
        }
        if (check.Ok)
        {
            row.Status = Counted("external.found", check.FromSource, ("where", check.MatchedRoot ?? check.Picked))
                + (check.FromSource < check.Found ? "\n" + Counted("external.kept", check.Found - check.FromSource) : "");
            return;
        }
        if (check.FromSource == 0) { row.Status = S["external.nothing_found"]; return; } // a pick without any of the files, whatever is stored
        var parts = new List<string>();
        if (check.Missing > 0) parts.Add(Counted("external.missing", check.Missing));
        if (check.Different > 0) parts.Add(Counted("external.different", check.Different));
        row.Status = check.MatchedRoot is null && check.Found == 0 ? S["external.nothing_found"] : string.Join(" · ", parts)
            + (check.MatchedRoot is { } where ? "\n" + S.Format("external.looked_in", ("where", where)) : "");
        row.Details = string.Join("\n", check.Files.Where(f => f.State != ExternalFileState.Ok).Take(200).Select(f =>
            S[f.State == ExternalFileState.Missing ? "external.file_missing" : "external.file_different"] + "  " + f.File.Path
            + "\n    " + S.Format("external.file_expected", ("bytes", f.File.Bytes.ToString("N0", CultureInfo.CurrentCulture)), ("sha", f.File.Sha256))));
    }

    /// <summary>Installs (or, when already installed, finishes the re-check of) the mod from the store, with the runtime its database base needs.</summary>
    public async Task InstallExternalAsync()
    {
        if (!CanInstallExternal || _externalMod is not { } mod) return;
        if (ExternalInstalled && _modHealth.Installed.FirstOrDefault(i => i.Id == mod.Id)?.Manifest?.SameContent(mod) == true)
        {
            // "Check files again": every stored copy is hashed; any that no longer matches must come from a new pick.
            IReadOnlyList<ExternalFile> failed = [];
            await RunModOperationAsync(async () =>
            {
                var manager = await Task.Run(ModManagerFactory()!);
                failed = await Task.Run(() => manager.VerifyStoredExternal(mod, null, CancellationToken.None));
            }, S["external.recheck_ok"]);
            if (failed.Count == 0) { if (Message == S["external.recheck_ok"]) CloseExternalSetup(); return; }
            foreach (var row in ExternalRows.Where(r => failed.Any(f => f.SourceId == r.Source.Id)))
            {
                var lost = failed.Where(f => f.SourceId == row.Source.Id).ToArray();
                row.Ok = false; row.Problems = true;
                row.Status = Counted("external.stored_damaged", lost.Length);
                row.Details = string.Join("\n", lost.Take(200).Select(f => S["external.file_missing"] + "  " + f.Path));
            }
            Message = S["mods.error.external_files"]; Changed(string.Empty);
            return;
        }
        var update = ExternalInstalled; var hardcore = ProfileHardcore;
        await RunModOperationAsync(async () =>
        {
            var progress = new Progress<string>(stage => { _modProgress = S["mods.progress." + stage]; Changed(nameof(ModProgress)); });
            if (mod.NeedsRuntime) await RequireRuntimeAsync();
            var manager = await Task.Run(ModManagerFactory()!);
            await Task.Run(() => manager.InstallExternal(mod, progress, update, hardcore));
            RecordModSource(mod.Id);
        }, S[update ? "mods.updated" : "mods.action_done"]);
        if (_modHealth.Installed.Any(i => i.Id == mod.Id && i.Manifest?.SameContent(mod) == true)) CloseExternalSetup();
    }

    // The launcher's picker (ShellViewModel.FilePicker.cs) ---------------------------------------------------------------

    /// <summary>Opens the picker for <paramref name="row"/>: a folder or a file as it accepts (ZIPs only beside folders), checked at once.</summary>
    public void StartBrowsing(ExternalSourceRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        OpenPicker(new(row.Label, row.PicksFolder, row.PicksFile, path => CheckExternalAsync(row, path))
        {
            Start = row.Picked,
            FileFilter = row.PicksFolder ? f => f.Extension.Equals(".zip", StringComparison.OrdinalIgnoreCase) : null,
        });
    }
}
