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
    public bool Ok { get => _ok; set => Set(ref _ok, value); }
    public bool Problems { get => _problems; set => Set(ref _problems, value); }
    public bool Checking { get => _checking; set => Set(ref _checking, value); }
    public double Progress { get => _progress; set => Set(ref _progress, value); }
    public string? Picked { get; set; }

    public event PropertyChangedEventHandler? PropertyChanged;
    private void Set<T>(ref T field, T value, [CallerMemberName] string? name = null) { field = value; Changed(name); }
    private void Changed(string? name) => PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(name));
}

/// <summary>An entry of the in-launcher browser: a folder, or a file the source accepts (a ZIP, or any file for a file source).</summary>
public sealed record BrowserEntry(string Name, string FullPath, bool Folder)
{
    public string Display => (Folder ? "📁  " : "🗜  ") + Name;
}

/// <summary>A quick location of the browser (Home, Downloads, SD card, a drive…), shown only when it exists.</summary>
public sealed record BrowserPlace(string Label, string Path);

/// <summary>
/// OD-117: the set-up pane of a mod made of the player's own files. One row per source; the player picks a folder (the pack or
/// any parent), a file or a ZIP with the in-launcher browser (controller-navigable, Steam keyboard on the path box, quick
/// locations that include Linux's home and SD cards under Wine) or, on Windows, the native dialog. Each pick is checked with
/// progress; Install is enabled only when every source verifies, after the short "your own copy" notice.
/// </summary>
public sealed partial class ShellViewModel
{
    private ModManifest? _externalMod;
    private bool _browsing;
    private ExternalSourceRow? _browsingRow;
    private string _browserPath = "";
    private string _browserDraft = "";
    private readonly Dictionary<ExternalSourceRow, CancellationTokenSource> _externalChecks = [];
    public ObservableCollection<ExternalSourceRow> ExternalRows { get; } = [];
    public ObservableCollection<BrowserEntry> BrowserEntries { get; } = [];
    public ObservableCollection<BrowserPlace> BrowserPlaces { get; } = [];

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
        : ExternalReplaced.Count > 0 ? S.Format("external.install_replacing", ("count", ExternalReplaced.Count.ToString(CultureInfo.CurrentCulture))) : S["external.install"];
    private bool ExternalUpdate => _externalMod is { } mod && _modHealth.Installed.FirstOrDefault(i => i.Id == mod.Id)?.Manifest?.SameContent(mod) == false;

    private (string Key, IReadOnlyList<string> Files)? _replaced;
    /// <summary>
    /// Before Install: the game folder the files go to and the existing files they replace (backed up, restored on disable or
    /// uninstall). Shown so a wrong folder — a real game picked for a test pack — is visible before anything is written.
    /// </summary>
    private IReadOnlyList<string> ExternalReplaced
    {
        get
        {
            if (_externalMod is not { } mod || ContentLocation?.Mount is not { } mount || ModManagerFactory() is not { } factory) return [];
            var key = mod.Digest + "|" + mount;
            if (_replaced?.Key != key)
                try { _replaced = (key, factory().ExistingFilesReplaced(mod)); }
                catch (Exception error) when (ManagedModFiles.IsStateError(error)) { _replaced = (key, []); }
            return _replaced.Value.Files;
        }
    }
    public string ExternalTargetText => _externalMod is { } mod && mod.Files.Any(f => f.Root == "game") && ContentLocation?.Mount is { } mount
        ? S.Format("external.target", ("path", mount)) : "";
    public bool ExternalReplaces => ExternalAllOk && ExternalReplaced.Count > 0;
    public string ExternalReplacesText => ExternalReplaced is { Count: > 0 } files
        ? S.Format("external.replaces", ("count", files.Count.ToString(CultureInfo.CurrentCulture)), ("files", string.Join("\n", files.Take(8)) + (files.Count > 8 ? "\n…" : ""))) : "";

    public bool ShowBrowser => _browsing && ShowExternalSetup;
    public string BrowserPath => _browserPath;
    public string BrowserDraft { get => _browserDraft; set { _browserDraft = value ?? ""; Changed(); } }
    public bool BrowserCanPickFolder => _browsingRow?.PicksFolder == true && Directory.Exists(_browserPath);
    public bool BrowserEmpty => BrowserEntries.Count == 0;
    /// <summary>Under Wine the Windows dialog shows Linux paths under Z: and is hard to use with a controller; only the browser is offered there.</summary>
    [System.Diagnostics.CodeAnalysis.SuppressMessage("Performance", "CA1822:Mark members as static", Justification = "XAML binds to instance properties")]
    public bool CanUseSystemPicker => !WineHost.Detected && !GamepadNavigator.SteamGamepadUi;

    /// <summary>
    /// Opens the pane for <paramref name="card"/>'s mod (<paramref name="update"/>: its reviewed pending update, from the update's own
    /// confirmation only): rows already copied show so; a remembered pick is checked again. "Check files again" never applies an update.
    /// </summary>
    public async Task OpenExternalSetupAsync(ModCard card, bool update = false)
    {
        ArgumentNullException.ThrowIfNull(card);
        var mod = update ? card.Next : card.Manifest;
        if (mod is not { IsExternal: true } || ModManagerFactory() is not { } factory || !ModsIdle || update && !card.CanAct) return;
        _externalMod = mod; _showModInfo = false; _showModOptions = false; _showLobbyPane = false; _browsing = false; _selectedMod = card;
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
                row.Ok = true; row.Status = S.Format("external.already", ("count", count.ToString(CultureInfo.CurrentCulture)));
            }
            else if (remembered.TryGetValue(row.Source.Id, out var path) && (Directory.Exists(path) || File.Exists(path))) await CheckExternalAsync(row, path);
        }
        Changed(string.Empty);
    }

    public void CloseExternalSetup()
    {
        foreach (var check in _externalChecks.Values) check.Cancel();
        _externalMod = null; _browsing = false; _browsingRow = null; _replaced = null;
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
        string N(int n) => n.ToString(CultureInfo.CurrentCulture);
        row.Ok = check.Ok; row.Problems = !check.Ok; row.Progress = 1;
        if (check.Ok)
        {
            row.Status = S.Format("external.found", ("count", N(check.FromSource)), ("where", check.MatchedRoot ?? check.Picked))
                + (check.FromSource < check.Found ? "\n" + S.Format("external.kept", ("count", N(check.Found - check.FromSource))) : "");
            return;
        }
        if (check.FromSource == 0) { row.Status = S["external.nothing_found"]; return; } // a pick without any of the files, whatever is stored
        var parts = new List<string>();
        if (check.Missing > 0) parts.Add(S.Format("external.missing", ("count", N(check.Missing))));
        if (check.Different > 0) parts.Add(S.Format("external.different", ("count", N(check.Different))));
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
                row.Status = S.Format("external.stored_damaged", ("count", lost.Length.ToString(CultureInfo.CurrentCulture)));
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

    // The in-launcher browser ------------------------------------------------------------------------------------------

    public void StartBrowsing(ExternalSourceRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        _browsingRow = row; _browsing = true;
        RefreshPlaces();
        var start = row.Picked is { } last && (Directory.Exists(last) || File.Exists(last)) ? (Directory.Exists(last) ? last : Path.GetDirectoryName(last)!)
            : BrowserPlaces.FirstOrDefault()?.Path ?? Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        Browse(start);
    }

    public void StopBrowsing() { _browsing = false; _browsingRow = null; BrowserEntries.Clear(); Changed(string.Empty); }

    /// <summary>Lists a folder: its subfolders, then the files the row may pick (ZIPs; any file for a file source). Unreadable falls back to Home.</summary>
    public void Browse(string path)
    {
        if (_browsingRow is not { } row) return;
        string full;
        try { full = Path.GetFullPath(path); } catch (Exception error) when (error is ArgumentException or NotSupportedException or PathTooLongException) { Message = S["external.browse_failed"]; return; }
        if (!Directory.Exists(full))
        {
            if (File.Exists(full) && row.PicksFile) { _ = PickAsync(full); return; }
            if (_browserPath.Length > 0) Message = S["external.browse_failed"]; // a typed path that does not exist says so; the first open falls back quietly
            full = Home();
        }
        var entries = new List<BrowserEntry>();
        try
        {
            var options = new EnumerationOptions { IgnoreInaccessible = true, AttributesToSkip = FileAttributes.System | FileAttributes.Hidden };
            entries.AddRange(new DirectoryInfo(full).EnumerateDirectories("*", options).Take(2000).OrderBy(d => d.Name, StringComparer.CurrentCultureIgnoreCase).Take(500)
                .Select(d => new BrowserEntry(d.Name, d.FullName, true)));
            if (row.PicksFile)
                entries.AddRange(new DirectoryInfo(full).EnumerateFiles("*", options)
                    .Where(f => row.PicksFolder ? f.Extension.Equals(".zip", StringComparison.OrdinalIgnoreCase) : true)
                    .Take(2000).OrderBy(f => f.Name, StringComparer.CurrentCultureIgnoreCase).Take(500).Select(f => new BrowserEntry(f.Name, f.FullName, false)));
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or System.Security.SecurityException)
        {
            if (!string.Equals(full, Home(), StringComparison.OrdinalIgnoreCase)) { Browse(Home()); return; }
        }
        _browserPath = full; _browserDraft = full;
        BrowserEntries.Clear();
        foreach (var entry in entries) BrowserEntries.Add(entry);
        Changed(string.Empty);
    }

    public void BrowseUp() { if (Path.GetDirectoryName(Path.TrimEndingDirectorySeparator(_browserPath)) is { } parent) Browse(parent); }
    public void BrowseDraft() => Browse(_browserDraft.Trim().Trim('"'));
    public Task PickBrowserFolderAsync() => BrowserCanPickFolder ? PickAsync(_browserPath) : Task.CompletedTask;
    public Task OpenBrowserEntryAsync(BrowserEntry entry) { ArgumentNullException.ThrowIfNull(entry); if (entry.Folder) { Browse(entry.FullPath); return Task.CompletedTask; } return PickAsync(entry.FullPath); }

    /// <summary>A pick from the browser or the Windows dialog: checked at once.</summary>
    public async Task PickAsync(string path)
    {
        if ((_browsingRow ?? ExternalRows.FirstOrDefault(r => !r.Ok)) is not { } target) return;
        _browsing = false; _browsingRow = null; BrowserEntries.Clear(); Changed(string.Empty);
        await CheckExternalAsync(target, path);
    }

    /// <summary>The Windows dialog's pick for <paramref name="row"/> (desktop Windows only).</summary>
    public Task PickWithSystemDialogAsync(ExternalSourceRow row, string path) { _browsingRow = row; return PickAsync(path); }

    private static string Home()
    {
        if (WineHost.Detected && WineHome() is { } linux && Directory.Exists(linux)) return linux;
        return Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
    }

    /// <summary>Under Wine, the player's Linux home through Z: (<c>WINEHOMEDIR</c> is <c>\??\Z:\home\deck</c>; else HOME or /home/USER).</summary>
    private static string? WineHome()
    {
        var wine = Environment.GetEnvironmentVariable("WINEHOMEDIR");
        if (wine is { Length: > 0 }) return wine.StartsWith(@"\??\", StringComparison.Ordinal) ? wine[4..] : wine;
        var home = Environment.GetEnvironmentVariable("HOME");
        if (home is { Length: > 1 } && home.StartsWith('/')) return @"Z:" + home.Replace('/', '\\');
        return Environment.GetEnvironmentVariable("USER") is { Length: > 0 } user ? @"Z:\home\" + user : null;
    }

    private void RefreshPlaces()
    {
        BrowserPlaces.Clear();
        void Add(string key, string? path, string? label = null) { if (path is { Length: > 0 } && Directory.Exists(path) && BrowserPlaces.All(p => !string.Equals(p.Path, path, StringComparison.OrdinalIgnoreCase))) BrowserPlaces.Add(new(label ?? S[key], path)); }
        var home = Home();
        Add("external.place_home", home);
        Add("external.place_downloads", Path.Combine(home, "Downloads"));
        Add("external.place_desktop", Path.Combine(home, "Desktop"));
        if (WineHost.Detected)
        {
            // SD cards and USB drives on a Deck / Linux desktop: /run/media/<user>/<card> (and /run/media/<card>), through Z:.
            try
            {
                const string media = @"Z:\run\media";
                if (Directory.Exists(media))
                    foreach (var level in Directory.EnumerateDirectories(media))
                    {
                        // /run/media/<user>/<card> on SteamOS and most desktops; /run/media/<card> elsewhere.
                        var children = Directory.EnumerateDirectories(level).ToArray();
                        // /run/media/<user>/<card> (SteamOS 3.5+, desktops) or /run/media/<card> (older SteamOS): only a user folder holds cards.
                        var user = Environment.GetEnvironmentVariable("USER");
                        foreach (var card in user is { Length: > 0 } && Path.GetFileName(level) == user ? children : [level])
                            Add("", card, S.Format("external.place_card", ("name", Path.GetFileName(card))));
                    }
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException) { }
        }
        foreach (var drive in DriveInfo.GetDrives())
            try { if (drive.DriveType is DriveType.Fixed or DriveType.Removable && drive.IsReady && !(WineHost.Detected && drive.Name.StartsWith("Z", StringComparison.OrdinalIgnoreCase))) Add("", drive.RootDirectory.FullName, S.Format("external.place_drive", ("name", drive.Name.TrimEnd('\\')))); }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException) { }
    }
}
