// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;

namespace Infinity.Core.Process;

/// <summary>
/// Opens a folder for the player. On Windows that is Explorer; under Wine/Proton it is the prefix's own <c>explorer.exe</c> (Wine's
/// file browser, present in every prefix and needing nothing from the host), since a host file manager reached through
/// <c>winebrowser</c>/<c>xdg-open</c> is not reliably available inside Steam's runtime container. The folder is created first.
/// </summary>
public static class FolderOpener
{
    public static void Open(string folder)
    {
        var full = Path.TrimEndingDirectorySeparator(Path.GetFullPath(folder));
        Directory.CreateDirectory(full);
        // Explicit explorer.exe and one argument: never a shell verb on a path, and no trailing separator to escape the quote.
        // Wine keeps its explorer.exe in the prefix's C:\windows as well.
        var start = new ProcessStartInfo(Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.Windows), "explorer.exe")) { UseShellExecute = false };
        start.ArgumentList.Add(full);
        System.Diagnostics.Process.Start(start)?.Dispose();
    }
}
