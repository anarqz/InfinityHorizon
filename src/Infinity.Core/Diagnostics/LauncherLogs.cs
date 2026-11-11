// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Globalization;
using System.IO.Compression;
using System.Text;
using System.Text.Json;
using Infinity.Core.Shell;

namespace Infinity.Core.Diagnostics;

/// <summary>Which button asked for the logs: only the zip's file name differs (OD-241).</summary>
public enum LogBundleKind { Crash, Bug }

/// <summary>What one "Get logs" zip collects.</summary>
/// <param name="AzimuthRoot">The Azimuth install; its <c>data\logs</c> (Azimuth's logs and the game's .dmp files) goes in whole.</param>
/// <param name="LauncherLogDirectories">Folders holding <c>launcher.log</c> and <c>launcher.previous-*.log</c>.</param>
/// <param name="Crashes">The launcher's local crash reports, projected with <see cref="CrashSupportReport"/> (and their stored dumps).</param>
public sealed record LogBundleRequest(LogBundleKind Kind, string? AzimuthRoot, IReadOnlyList<string> LauncherLogDirectories,
    CrashIncidentStore? Crashes, string LauncherVersion, bool Wine);

/// <summary>The zip written: its full path and the entry names in it (and what was left out, by name).</summary>
public sealed record LogBundle(string Path, IReadOnlyList<string> Entries, IReadOnlyList<string> Skipped);

/// <summary>
/// OD-241: the one collector behind both entry points (the crash screen's "Get logs" and "Found a bug?"'s "Get launcher logs"). Text
/// goes through <see cref="LogRedactor"/>; a minidump goes in as it is; any other binary file (packet captures, caches) is left out.
/// </summary>
public static class LauncherLogCollector
{
    public const int MaxFiles = 256;
    public const long MaxTextBytes = 16L * 1024 * 1024;
    public const long MaxTotalBytes = 256L * 1024 * 1024;
    private static readonly JsonSerializerOptions Indented = new() { WriteIndented = true };
    private static readonly string[] TextExtensions = [".log", ".txt", ".json", ".ini", ".cfg", ".csv", ".xml"];

    /// <summary><c>infinity-crashdump-{yyyy-MM-dd HH-mm-ss}.zip</c> or <c>launcher-logs-{yyyy-MM-dd HH-mm-ss}.zip</c>.</summary>
    public static string FileName(LogBundleKind kind, DateTime local) =>
        (kind == LogBundleKind.Crash ? "infinity-crashdump-" : "launcher-logs-") + local.ToString("yyyy-MM-dd HH-mm-ss", CultureInfo.InvariantCulture) + ".zip";

    public static LogBundle Write(LogBundleRequest request, string folder, DateTime local)
    {
        ArgumentNullException.ThrowIfNull(request);
        Directory.CreateDirectory(folder);
        var target = FreeName(folder, FileName(request.Kind, local));
        var partial = target + ".partial";
        var entries = new List<string>();
        var skipped = new List<string>();
        long total = 0;
        try
        {
            using (var stream = new FileStream(partial, FileMode.Create, FileAccess.Write, FileShare.None))
            using (var zip = new ZipArchive(stream, ZipArchiveMode.Create))
            {
                bool Room(long length, string name)
                {
                    if (entries.Count < MaxFiles && total + length <= MaxTotalBytes) { total += length; return true; }
                    skipped.Add(name + " (size limit)");
                    return false;
                }
                void Text(string name, string text)
                {
                    var bytes = Encoding.UTF8.GetBytes(LogRedactor.Redact(text));
                    if (!Room(bytes.Length, name)) return;
                    using var entry = zip.CreateEntry(name, CompressionLevel.Optimal).Open();
                    entry.Write(bytes);
                    entries.Add(name);
                }
                void Binary(string name, byte[] bytes)
                {
                    if (!Room(bytes.Length, name)) return;
                    using var entry = zip.CreateEntry(name, CompressionLevel.Optimal).Open();
                    entry.Write(bytes);
                    entries.Add(name);
                }

                // Azimuth's data\logs, the game's dumps included
                if (request.AzimuthRoot is { Length: > 0 } root)
                {
                    var logs = Path.Combine(root, "data", "logs");
                    foreach (var file in Files(logs, skipped, "azimuth-logs/"))
                    {
                        var name = "azimuth-logs/" + file.Name;
                        if (file.Extension.Equals(".dmp", StringComparison.OrdinalIgnoreCase))
                        {
                            if (file.Length > CrashDumpCollector.MaxDumpBytes) { skipped.Add(name + " (size limit)"); continue; }
                            if (Read(file, skipped, name) is { } dump) Binary(name, dump);
                        }
                        else if (TextExtensions.Contains(file.Extension.ToLowerInvariant()))
                        {
                            if (file.Length > MaxTextBytes) { skipped.Add(name + " (size limit)"); continue; }
                            if (Read(file, skipped, name) is { } bytes) Text(name, Encoding.UTF8.GetString(bytes));
                        }
                        else skipped.Add(name + " (not a log)");
                    }
                }

                // launcher.log and launcher.previous-*.log
                var folders = request.LauncherLogDirectories.Where(d => !string.IsNullOrEmpty(d)).Select(Path.GetFullPath)
                    .Distinct(StringComparer.OrdinalIgnoreCase).ToArray();
                for (var i = 0; i < folders.Length; i++)
                {
                    var prefix = i == 0 ? "launcher/" : $"launcher-{i + 1}/";
                    foreach (var file in Files(folders[i], skipped, prefix)
                        .Where(f => f.Name.Equals("launcher.log", StringComparison.OrdinalIgnoreCase) || IsPreviousLog(f.Name)))
                    {
                        if (file.Length > MaxTextBytes) { skipped.Add(prefix + file.Name + " (size limit)"); continue; }
                        if (Read(file, skipped, prefix + file.Name) is { } bytes) Text(prefix + file.Name, Encoding.UTF8.GetString(bytes));
                    }
                }

                // the launcher's own crash diagnostics: the support projection of each local report, and its stored dump
                if (request.Crashes is { } crashes)
                {
                    IReadOnlyList<CrashIncident> incidents;
                    try { incidents = crashes.All(); }
                    catch (Exception e) when (e is IOException or UnauthorizedAccessException) { incidents = []; skipped.Add("crash-reports (" + e.GetType().Name + ")"); }
                    foreach (var incident in incidents)
                    {
                        Text($"crash-reports/{incident.Id}.json", CrashSupportReport.Create(incident));
                        try { if (crashes.ReadDump(incident) is { } dump) Binary($"crash-reports/{incident.Id}.dmp", dump); }
                        catch (Exception e) when (e is IOException or UnauthorizedAccessException or InvalidDataException) { skipped.Add($"crash-reports/{incident.Id}.dmp ({e.GetType().Name})"); }
                    }
                }

                var manifest = JsonSerializer.Serialize(new
                {
                    format = "infinity-logs-v1",
                    kind = request.Kind == LogBundleKind.Crash ? "crash" : "bug",
                    createdAt = local.ToString("yyyy-MM-dd'T'HH:mm:ss", CultureInfo.InvariantCulture),
                    launcherVersion = request.LauncherVersion.Length <= 64 ? request.LauncherVersion : "unknown",
                    platform = request.Wine ? "wine" : "windows",
                    entries,
                    skipped,
                }, Indented);
                using (var entry = zip.CreateEntry("bundle.json", CompressionLevel.Optimal).Open())
                    entry.Write(Encoding.UTF8.GetBytes(LogRedactor.Redact(manifest)));
            }
            File.Move(partial, target);
        }
        catch
        {
            try { File.Delete(partial); } catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
            throw;
        }
        return new(target, entries, skipped);
    }

    internal static bool IsPreviousLog(string name) =>
        name.StartsWith("launcher.previous-", StringComparison.OrdinalIgnoreCase) && name.EndsWith(".log", StringComparison.OrdinalIgnoreCase)
        && name["launcher.previous-".Length..^4] is { Length: >= 1 and <= 3 } number && number.All(char.IsAsciiDigit);

    private static IEnumerable<FileInfo> Files(string folder, List<string> skipped, string prefix)
    {
        var directory = new DirectoryInfo(folder);
        if (!directory.Exists) return [];
        if ((directory.Attributes & FileAttributes.ReparsePoint) != 0) { skipped.Add(prefix + " (link)"); return []; }
        var list = new List<FileInfo>();
        foreach (var file in directory.EnumerateFiles().Take(MaxFiles))
        {
            if ((file.Attributes & FileAttributes.ReparsePoint) != 0) { skipped.Add(prefix + file.Name + " (link)"); continue; }
            list.Add(file);
        }
        return list.OrderBy(f => f.Name, StringComparer.OrdinalIgnoreCase);
    }

    private static byte[]? Read(FileInfo file, List<string> skipped, string name)
    {
        // a log may be open for writing (launcher.log is this process's own)
        try
        {
            using var stream = new FileStream(file.FullName, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
            using var copy = new MemoryStream();
            stream.CopyTo(copy);
            return copy.ToArray();
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { skipped.Add(name + " (" + e.GetType().Name + ")"); return null; }
    }

    private static string FreeName(string folder, string name)
    {
        var path = Path.Combine(folder, name);
        for (var i = 2; File.Exists(path) || File.Exists(path + ".partial"); i++)
            path = Path.Combine(folder, Path.GetFileNameWithoutExtension(name) + $" ({i})" + Path.GetExtension(name));
        return path;
    }
}

/// <summary>Where the zip went: the host's real Desktop, found in this order.</summary>
public enum DesktopSource { Windows, XdgUserDirs, HomeDesktop, PrefixDesktop }

public sealed record DesktopChoice(string Folder, DesktopSource Source);

public enum ClipboardOutcome { Copied, Failed, NotOnWindows }

public enum HelpChannelOutcome { App, Web, Failed }

public enum FolderOutcome { Selected, Opened, Failed }

public sealed record LogsDelivery(ClipboardOutcome Clipboard, HelpChannelOutcome Discord, FolderOutcome Folder);

/// <summary>The PC as the "Get logs" steps see it: tests replace every member (no real Desktop, clipboard, Discord or Explorer).</summary>
public sealed class LogsHost
{
    /// <summary>Running under Wine/Proton (<c>WineHost.Detected</c>).</summary>
    public required bool Wine { get; init; }
    /// <summary>The environment Wine passes through (<c>HOME</c>).</summary>
    public required Func<string, string?> Environment { get; init; }
    public required Func<string, bool> DirectoryExists { get; init; }
    /// <summary>A small text file's content, or null when it cannot be read.</summary>
    public required Func<string, string?> ReadText { get; init; }
    /// <summary><c>Environment.SpecialFolder.Desktop</c>: the real Desktop on Windows, the prefix's plain folder under Wine.</summary>
    public required Func<string> SpecialDesktop { get; init; }
    /// <summary>Puts the file on the clipboard as a file (CF_HDROP); false when that failed. Never called under Wine.</summary>
    public required Func<string, bool> CopyFileToClipboard { get; init; }
    /// <summary>Starts a process or opens a link; false when it could not.</summary>
    public required Func<ProcessStartInfo, bool> Start { get; init; }
    /// <summary>The Discord app registered its <c>discord://</c> link handler.</summary>
    public required Func<bool> DiscordHandlerRegistered { get; init; }
    public Func<DateTime> Now { get; init; } = () => DateTime.Now;
}

public static class LogsHandoff
{
    /// <summary>
    /// The host's real Desktop. Windows: <c>SpecialFolder.Desktop</c>. Under Wine/Proton the prefix's <c>drive_c\users\steamuser\Desktop</c>
    /// is a plain folder the player never sees, so: (1) <c>XDG_DESKTOP_DIR</c> of <c>$HOME/.config/user-dirs.dirs</c>, (2) <c>$HOME/Desktop</c>,
    /// both through Wine's <c>Z:</c> drive (the Linux root); only when neither exists (or HOME is missing) the prefix's Desktop.
    /// </summary>
    public static DesktopChoice Desktop(LogsHost host)
    {
        ArgumentNullException.ThrowIfNull(host);
        if (!host.Wine) return new(host.SpecialDesktop(), DesktopSource.Windows);
        if (host.Environment("HOME") is { Length: > 1 } home && home.StartsWith('/'))
        {
            home = home.TrimEnd('/');
            if (host.ReadText(ToWine(home + "/.config/user-dirs.dirs")) is { } dirs && XdgDesktop(dirs, home) is { } xdg
                && host.DirectoryExists(ToWine(xdg))) return new(ToWine(xdg), DesktopSource.XdgUserDirs);
            if (host.DirectoryExists(ToWine(home + "/Desktop"))) return new(ToWine(home + "/Desktop"), DesktopSource.HomeDesktop);
        }
        return new(host.SpecialDesktop(), DesktopSource.PrefixDesktop);
    }

    /// <summary><c>XDG_DESKTOP_DIR</c> from a <c>user-dirs.dirs</c> text: quotes removed, <c>$HOME</c>/<c>${HOME}</c> expanded; absolute only.</summary>
    internal static string? XdgDesktop(string dirs, string home)
    {
        string? found = null;
        foreach (var raw in dirs.Split('\n'))
        {
            var line = raw.Trim();
            if (line.Length == 0 || line[0] == '#') continue;
            var equals = line.IndexOf('=', StringComparison.Ordinal);
            if (equals < 0 || line[..equals].Trim() != "XDG_DESKTOP_DIR") continue;
            var value = line[(equals + 1)..].Trim();
            if (value.Length >= 2 && (value[0] == '"' && value[^1] == '"' || value[0] == '\'' && value[^1] == '\'')) value = value[1..^1];
            value = value.Replace("\\\"", "\"", StringComparison.Ordinal).Replace("${HOME}", home, StringComparison.Ordinal).Replace("$HOME", home, StringComparison.Ordinal);
            found = value; // the last assignment wins, as in the shell that sources it
        }
        if (found is null || !found.StartsWith('/') || found.Contains('\0', StringComparison.Ordinal)) return null;
        found = found.TrimEnd('/');
        return found.Length == 0 ? null : found;
    }

    /// <summary>A Linux path as Wine shows it: <c>Z:</c> is the Linux root (<c>/home/u/Desktop</c> → <c>Z:\home\u\Desktop</c>).</summary>
    public static string ToWine(string linux) => "Z:" + linux.Replace('/', '\\');

    /// <summary>Resolves the Desktop and writes the zip there (off the UI thread).</summary>
    public static (LogBundle Bundle, DesktopChoice Desktop) Collect(LogBundleRequest request, LogsHost host)
    {
        ArgumentNullException.ThrowIfNull(host);
        var desktop = Desktop(host);
        return (LauncherLogCollector.Write(request, desktop.Folder, host.Now()), desktop);
    }

    /// <summary>
    /// After the zip is written (on the UI thread, for the clipboard): the zip on the clipboard as a file (Windows only), Discord #help,
    /// and the folder with the zip selected (Windows) or the folder (Wine: <c>start /unix xdg-open</c>, no selection there).
    /// </summary>
    public static LogsDelivery Deliver(string zip, LogsHost host)
    {
        ArgumentNullException.ThrowIfNull(host);
        var clipboard = host.Wine ? ClipboardOutcome.NotOnWindows : host.CopyFileToClipboard(zip) ? ClipboardOutcome.Copied : ClipboardOutcome.Failed;
        return new(clipboard, OpenHelp(host), OpenFolder(zip, host));
    }

    /// <summary>OD-241: Discord #help (<see cref="SupportLinks.Help"/>), the app when its handler is there, else the browser.</summary>
    public static HelpChannelOutcome OpenHelp(LogsHost host)
    {
        ArgumentNullException.ThrowIfNull(host);
        var channel = SupportLinks.Help;
        if (host.DiscordHandlerRegistered() && host.Start(new ProcessStartInfo(channel.App.AbsoluteUri) { UseShellExecute = true })) return HelpChannelOutcome.App;
        return host.Start(new ProcessStartInfo(channel.Web.AbsoluteUri) { UseShellExecute = true }) ? HelpChannelOutcome.Web : HelpChannelOutcome.Failed;
    }

    internal static ProcessStartInfo FolderCommand(string zip, bool wine)
    {
        if (!wine)
        {
            // one argument as Explorer parses it: /select,"<zip>"
            return new ProcessStartInfo("explorer.exe", "/select,\"" + zip + "\"") { UseShellExecute = false };
        }
        var start = new ProcessStartInfo("start.exe") { UseShellExecute = false };
        start.ArgumentList.Add("/unix");
        start.ArgumentList.Add("/usr/bin/xdg-open");
        start.ArgumentList.Add(LinuxPath(Path.GetDirectoryName(zip) ?? zip));
        return start;
    }

    private static FolderOutcome OpenFolder(string zip, LogsHost host) =>
        host.Start(FolderCommand(zip, host.Wine)) ? host.Wine ? FolderOutcome.Opened : FolderOutcome.Selected : FolderOutcome.Failed;

    /// <summary>The Linux path of a Wine path on <c>Z:</c>; a prefix path (<c>C:</c>) through <c>WineHost.UnixPath</c>.</summary>
    private static string LinuxPath(string windows) =>
        windows.Length >= 3 && char.ToUpperInvariant(windows[0]) == 'Z' && windows[1] == ':'
            ? windows[2..].Replace('\\', '/')
            : Infinity.Core.Process.WineHost.UnixPath(windows) ?? windows;
}
