// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Specialized;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using Infinity.Core.Diagnostics;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>
/// OD-241: logs go to Discord #help by hand. One collector serves the crash screen's "Get logs" and "Found a bug?"'s "Get launcher logs":
/// the zip on the real Desktop, on the clipboard as a file (Windows), Discord #help opened, the folder shown, and the notice with the
/// path. No server, no GitHub, no upload.
/// </summary>
public sealed partial class ShellViewModel
{
    private string? _logsStatus;

    /// <summary>Tests and the harness only: the PC as the log steps see it (no real Desktop, clipboard, Discord or Explorer).</summary>
    internal LogsHost? LogsHostOverride { get; set; }

    public string LogsStatus => _logsStatus ?? string.Empty;
    public bool HasLogsStatus => _logsStatus is { Length: > 0 };
    public bool CanGetLauncherLogs => !_busy;

    /// <summary>"Found a bug?" → "Get launcher logs".</summary>
    public async Task GetLauncherLogsAsync()
    {
        if (!CanGetLauncherLogs) return;
        await CollectLogsAsync(LogBundleKind.Bug);
        Changed(string.Empty);
    }

    /// <summary>The integrity card's support link: Discord #help (app, else browser); a failure says where to go.</summary>
    public void OpenHelpChannel()
    {
        var outcome = LogsHandoff.OpenHelp(LogsHostOverride ?? SystemLogsHost());
        Log("help: #help " + outcome);
        if (outcome == HelpChannelOutcome.Failed) Message = S.Format("logs.discord_failed", ("url", Infinity.Core.Shell.SupportLinks.Help.Web.AbsoluteUri));
    }

    /// <summary>The one path of both buttons, under the busy overlay; true when the zip was written.</summary>
    internal async Task<bool> CollectLogsAsync(LogBundleKind kind)
    {
        if (_busy) return false;
        var saved = false;
        await Run(S["logs.collecting"], async () =>
        {
            var host = LogsHostOverride ?? SystemLogsHost();
            var request = new LogBundleRequest(kind, _install?.Root, [_stateDirectory, _stateRoot], CrashStore, Version, host.Wine);
            Log("logs: collecting (" + (kind == LogBundleKind.Crash ? "crash" : "bug") + ")");
            LogBundle bundle;
            DesktopChoice desktop;
            try { (bundle, desktop) = await Task.Run(() => LogsHandoff.Collect(request, host)); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException or System.Security.SecurityException)
            {
                Log("logs: zip not written (" + e.GetType().Name + ")");
                _logsStatus = Message = S["logs.failed"];
                return;
            }
            Log($"logs: zip written, {bundle.Entries.Count} file(s), {bundle.Skipped.Count} left out, desktop {desktop.Source}");
            var delivery = LogsHandoff.Deliver(bundle.Path, host);
            Log($"logs: clipboard {delivery.Clipboard}, #help {delivery.Discord}, folder {delivery.Folder}");
            _logsStatus = Message = LogsNotice(bundle.Path, delivery);
            saved = true;
        });
        return saved;
    }

    /// <summary>The notice says "copied" only when the clipboard took the file; it always shows where the zip is.</summary>
    internal string LogsNotice(string path, LogsDelivery delivery)
    {
        var notice = S.Format(delivery.Clipboard switch
        {
            ClipboardOutcome.Copied => "logs.saved_copied",
            ClipboardOutcome.Failed => "logs.saved_copy_failed",
            _ => "logs.saved_not_copied",
        }, ("path", path));
        if (delivery.Discord == HelpChannelOutcome.Failed)
            notice += " " + S.Format("logs.discord_failed", ("url", Infinity.Core.Shell.SupportLinks.Help.Web.AbsoluteUri));
        return notice;
    }

    private static LogsHost SystemLogsHost() => new()
    {
        Wine = WineHost.Detected,
        Environment = System.Environment.GetEnvironmentVariable,
        DirectoryExists = Directory.Exists,
        ReadText = path =>
        {
            try
            {
                var info = new FileInfo(path);
                return info.Exists && info.Length <= 64 * 1024 ? File.ReadAllText(path) : null;
            }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException or ArgumentException or NotSupportedException) { return null; }
        },
        SpecialDesktop = () => System.Environment.GetFolderPath(System.Environment.SpecialFolder.Desktop, System.Environment.SpecialFolderOption.Create) is { Length: > 0 } desktop
            ? desktop : Path.Combine(System.Environment.GetFolderPath(System.Environment.SpecialFolder.UserProfile), "Desktop"),
        CopyFileToClipboard = path =>
        {
            try { System.Windows.Clipboard.SetFileDropList(new StringCollection { path }); return true; }
            catch (Exception e) when (e is System.Runtime.InteropServices.ExternalException or ArgumentException or InvalidOperationException) { return false; }
        },
        Start = start =>
        {
            try { Process.Start(start)?.Dispose(); return true; }
            catch (Exception e) when (e is Win32Exception or InvalidOperationException or FileNotFoundException or PlatformNotSupportedException) { return false; }
        },
        DiscordHandlerRegistered = DiscordHandlerRegistered,
    };

    /// <summary>The Discord app registers <c>discord://</c> as a URL protocol (HKCU or HKLM classes).</summary>
    private static bool DiscordHandlerRegistered()
    {
        try
        {
            using var key = Microsoft.Win32.Registry.ClassesRoot.OpenSubKey("discord");
            return key?.GetValue("URL Protocol") is not null;
        }
        catch (Exception e) when (e is System.Security.SecurityException or UnauthorizedAccessException or IOException) { return false; }
    }
}
