// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.IO;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>
/// What the in-launcher picker is asked for: a folder, a file or either, with an optional filter for the files listed, where to
/// start, the label of "pick this folder", and what to do with the pick (the caller's own check or step).
/// </summary>
public sealed record FilePickRequest(string Title, bool PicksFolder, bool PicksFile, Func<string, Task> OnPick)
{
    public string? Start { get; init; }
    public Func<FileInfo, bool>? FileFilter { get; init; }
    public string? PickFolderLabel { get; init; }
    /// <summary>Closed without a pick (Cancel, or another pick asked for meanwhile).</summary>
    public Action? OnCancel { get; init; }
}

/// <summary>
/// Owner 2026-10-28: every folder or file the launcher asks for is chosen in its own picker, a pane on the right (never the
/// Windows dialog): controller-navigable, the Steam keyboard on the path box, quick locations that include Linux's home and SD
/// cards under Wine. A pick goes back to the caller.
/// </summary>
public sealed partial class ShellViewModel
{
    private FilePickRequest? _pick;
    private string _browserPath = "";
    private string _browserDraft = "";
    public ObservableCollection<BrowserEntry> BrowserEntries { get; } = [];
    public ObservableCollection<BrowserPlace> BrowserPlaces { get; } = [];

    public bool ShowBrowser => _pick is not null;
    public string BrowserTitle => _pick?.Title ?? "";
    public string BrowserPath => _browserPath;
    public string BrowserDraft { get => _browserDraft; set { _browserDraft = value ?? ""; Changed(); } }
    public bool BrowserPicksFolder => _pick?.PicksFolder == true;
    public string BrowserPickLabel => _pick?.PickFolderLabel ?? S["external.pick_this"];
    public bool BrowserCanPickFolder => _pick?.PicksFolder == true && Directory.Exists(_browserPath);
    public bool BrowserEmpty => BrowserEntries.Count == 0;

    /// <summary>Opens the picker for <paramref name="request"/>, at its start folder (or the first quick location).</summary>
    public void OpenPicker(FilePickRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        if (_pick is not null) StopBrowsing();
        _pick = request; _browserPath = "";
        RefreshPlaces();
        var start = request.Start is { } last && (Directory.Exists(last) || File.Exists(last)) ? (Directory.Exists(last) ? last : Path.GetDirectoryName(last)!)
            : BrowserPlaces.FirstOrDefault()?.Path ?? Environment.GetFolderPath(Environment.SpecialFolder.UserProfile);
        Browse(start);
    }

    /// <summary>Closes the picker without a pick (Cancel, Escape).</summary>
    public void StopBrowsing() { var request = _pick; ClosePicker(); request?.OnCancel?.Invoke(); }

    private void ClosePicker() { _pick = null; _browserPath = ""; BrowserEntries.Clear(); Changed(string.Empty); }

    /// <summary>Lists a folder: its subfolders, then the files the request may pick. Unreadable falls back to Home.</summary>
    public void Browse(string path)
    {
        if (_pick is not { } request) return;
        string full;
        try { full = Path.GetFullPath(path); } catch (Exception error) when (error is ArgumentException or NotSupportedException or PathTooLongException) { Message = S["external.browse_failed"]; return; }
        if (!Directory.Exists(full))
        {
            if (File.Exists(full) && request.PicksFile) { _ = PickAsync(full); return; }
            if (_browserPath.Length > 0) Message = S["external.browse_failed"]; // a typed path that does not exist says so; the first open falls back quietly
            full = Home();
        }
        var entries = new List<BrowserEntry>();
        try
        {
            var options = new EnumerationOptions { IgnoreInaccessible = true, AttributesToSkip = FileAttributes.System | FileAttributes.Hidden };
            entries.AddRange(new DirectoryInfo(full).EnumerateDirectories("*", options).Take(2000).OrderBy(d => d.Name, StringComparer.CurrentCultureIgnoreCase).Take(500)
                .Select(d => new BrowserEntry(d.Name, d.FullName, true)));
            if (request.PicksFile)
                entries.AddRange(new DirectoryInfo(full).EnumerateFiles("*", options).Where(f => request.FileFilter?.Invoke(f) ?? true)
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

    /// <summary>A pick: the picker closes and the caller gets the path.</summary>
    public async Task PickAsync(string path)
    {
        if (_pick is not { } request) return;
        ClosePicker();
        await request.OnPick(path);
    }

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

    // The callers --------------------------------------------------------------------------------------------------------

    /// <summary>The Azimuth folder (Configure, Mods): the folder that has azimuth.exe.</summary>
    public void ChooseAzimuthWithPicker() => OpenPicker(new(S["setup.choose_title"], true, false, folder => { ChooseAzimuthFolder(folder); return Task.CompletedTask; })
    { Start = _install?.Root });

    /// <summary>The Azimuth folder on the setup shown before sign-in (owner 2026-10-27).</summary>
    public void ChooseAzimuthFirstWithPicker() => OpenPicker(new(S["setup.choose_title"], true, false, folder => { ChooseAzimuthFirst(folder); return Task.CompletedTask; })
    { Start = _install?.Root });

    /// <summary>The game package folder (or Mount itself) for the integrity check and mods that write game files.</summary>
    public void ChooseContentWithPicker() => OpenPicker(new(S["integrity.choose_title"], true, false, ChooseContentFolderAsync) { Start = ContentLocation?.PackageRoot });

    /// <summary>The diagnostic file: saved under its own name in the folder picked (never over another file).</summary>
    public void SaveCrashReportWithPicker()
    {
        if (CopyableCrashReport() is not { } report) return;
        OpenPicker(new(S["crash.save_report"], true, false, folder =>
        {
            try { File.WriteAllText(FreeFileName(folder, "infinity-crash-report.json"), report, new System.Text.UTF8Encoding(false)); CrashExportResult(true); }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException) { CrashExportResult(false); }
            return Task.CompletedTask;
        }) { PickFolderLabel = S["picker.save_here"] });
    }

    /// <summary>The private crash dump, saved locally in the folder picked; a cancelled pick ends the export as before.</summary>
    internal void SaveCrashDumpWithPicker()
    {
        if (BeginManualCrashDumpExport() is not { } operation) return;
        OpenPicker(new(S["crash.save_dump"], true, false, async folder =>
        {
            bool? success = null;
            try
            {
                if (!IsCurrentCrashExport(operation)) return;
                success = false;
                var bytes = await ReadCrashDumpForManualExportAsync(operation);
                if (bytes is null) return;
                await File.WriteAllBytesAsync(FreeFileName(folder, "infinity-private-crash.dmp"), bytes, operation.Stop.Token);
                success = true;
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException or OperationCanceledException) { success = false; }
            finally { FinishManualCrashDumpExport(operation, success); }
        }) { PickFolderLabel = S["picker.save_here"], OnCancel = () => FinishManualCrashDumpExport(operation, null) });
    }

    /// <summary>A free file name in <paramref name="folder"/>: <paramref name="name"/>, else "name (2).ext", "name (3).ext"… (an export never overwrites).</summary>
    internal static string FreeFileName(string folder, string name)
    {
        var path = Path.Combine(folder, name);
        for (var n = 2; File.Exists(path) && n < 1000; n++) path = Path.Combine(folder, $"{Path.GetFileNameWithoutExtension(name)} ({n}){Path.GetExtension(name)}");
        return path;
    }
}
