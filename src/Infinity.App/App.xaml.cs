// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Threading;

namespace Infinity.App;

public partial class App : Application
{
    public App()
    {
        // Under Wine/Proton WPF's Direct3D 9 path leaves stale regions on screen (a pane drawn over
        // another until the mouse moves); software rendering draws every frame correctly there.
        if (Infinity.Core.Process.WineHost.Detected)
        {
            RenderOptions.ProcessRenderMode = RenderMode.SoftwareOnly;
        }
        // Owner 2026-11-10 (the window never freezes): the save scans hash large files on workers; a blocking full collection would
        // stop the window's thread too. This mode keeps the full collections concurrent (ephemeral ones stay short) unless memory runs low.
        System.Runtime.GCSettings.LatencyMode = System.Runtime.GCLatencyMode.SustainedLowLatency;
        // A fault in one action never takes the launcher (and the play session it supervises) down:
        // it is written to launcher.log and the shell keeps running.
        DispatcherUnhandledException += OnDispatcherUnhandled;
        TaskScheduler.UnobservedTaskException += (_, e) =>
        {
            Log("unobserved task: " + e.Exception);
            e.SetObserved();
        };
        // T4b: why the launcher ended, so a log never just stops (a fatal fault, or which way it was closed). No personal data: the
        // reason and the exit code only; a fault's exception as the other handlers log it.
        AppDomain.CurrentDomain.UnhandledException += (_, e) => Log("fatal: " + e.ExceptionObject + (e.IsTerminating ? " (terminating)" : ""));
        Exit += (_, e) =>
        {
            Log("exit: " + ExitReason + ", code " + e.ApplicationExitCode.ToString(System.Globalization.CultureInfo.InvariantCulture) + ExitMemory());
            GuardExit(e.ApplicationExitCode);
        };
        SessionEnding += (_, e) => ExitReason = "Windows session ending (" + e.ReasonSessionEnding + ")";
    }

    /// <summary>
    /// CR-L41.2: a launch carrying an <c>infinity-horizon://join/…</c> link while another launcher holds <c>launcher.lock</c> hands the
    /// link to it and ends here, before any window. With no launcher running it starts normally and takes the link itself.
    /// </summary>
    protected override void OnStartup(StartupEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        if (Infinity.Core.Presence.JoinLink.FromArguments(e.Args) is { } worldId && OperatingSystem.IsWindows())
        {
            string? state = null;
            try { state = Infinity.App.MainWindow.ConfigFor(e.Args).StateDirectory(Infinity.App.MainWindow.StateRoot); }
            catch (Exception error) when (error is ArgumentException or InvalidOperationException or IOException or UriFormatException)
            { Log("link: hand-off skipped (" + error.GetType().Name + ")"); }
            if (state is not null && Infinity.Core.Process.LauncherHandoff.TrySend(state, worldId))
            {
                ExitReason = "join link handed to the running launcher";
                StartupUri = null;
                base.OnStartup(e);
                Shutdown(0);
                return;
            }
        }
        // owner 2026-11-10 (the first frame): the splash on its own thread at once, before the main window is built; it closes when the
        // main window drew its first frame (MainWindow), or at the latest when the launcher ends
        StartSplash.Show(Infinity.App.MainWindow.LocaleOf(e.Args));
        Exit += (_, _) => StartSplash.Close();
        base.OnStartup(e);
    }

    /// <summary>How the launcher is ending, for the exit line in launcher.log; the window close is the default.</summary>
    internal static string ExitReason { get; set; } = "window closed";

    /// <summary>How long the process may still run after the exit line before it is ended by force.</summary>
    internal static TimeSpan ExitGrace { get; set; } = TimeSpan.FromSeconds(10);

    /// <summary>T-1118: what the process held when it exited (numbers only), so a later look at a process that never ended is dated.</summary>
    private static string ExitMemory()
    {
        try
        {
            var m = Infinity.Core.Diagnostics.MemorySample.Current();
            static string Mb(long bytes) => (bytes / (1024 * 1024)).ToString(System.Globalization.CultureInfo.InvariantCulture);
            return "; memory: private " + Mb(m.PrivateBytes) + " MB (managed heap " + Mb(m.ManagedHeap) + " MB, gc committed " + Mb(m.GcCommitted) + " MB), handles "
                + m.Handles.ToString(System.Globalization.CultureInfo.InvariantCulture) + ", threads " + m.Threads.ToString(System.Globalization.CultureInfo.InvariantCulture);
        }
        catch (Exception e) when (e is InvalidOperationException or System.ComponentModel.Win32Exception or NotSupportedException) { return string.Empty; }
    }

    /// <summary>
    /// T-1118: on 2026-11-18 a launcher process started the day before was still running with no window and about 20 GB: its window had
    /// been closed while its thread was held (a notify-all storm), so the application's shutdown never ran and no exit line was written.
    /// Once the window is closed (and the view model disposed), or the exit line written, nothing of the launcher is left to do: a
    /// process still running <see cref="ExitGrace"/> later says so in the log and ends itself. The guard is a background thread: it
    /// never keeps the process alive, and a normal exit (under 0.1 s) ends it first.
    /// </summary>
    internal static void GuardExit(int code, string after = "its exit")
    {
        var grace = ExitGrace;
        try
        {
            new System.Threading.Thread(() =>
            {
                System.Threading.Thread.Sleep(grace);
                Log("exit: the process was still running " + ((long)grace.TotalSeconds).ToString(System.Globalization.CultureInfo.InvariantCulture) + " s after " + after + "; ended by force" + ExitMemory());
                // TerminateProcess: no shutdown code of any library runs again (that is what did not finish)
                try { using var self = System.Diagnostics.Process.GetCurrentProcess(); self.Kill(); }
                catch (Exception e) when (e is InvalidOperationException or System.ComponentModel.Win32Exception) { Environment.Exit(code); }
            }) { IsBackground = true, Name = "exit-guard" }.Start();
        }
        catch (Exception e) when (e is OutOfMemoryException or System.Threading.ThreadStateException) { Log("exit: guard not started (" + e.GetType().Name + ")"); }
    }

    private static void OnDispatcherUnhandled(object sender, DispatcherUnhandledExceptionEventArgs e)
    {
        Log("unhandled: " + e.Exception);
        if (Current?.MainWindow?.DataContext is ShellViewModel vm)
        {
            vm.Message = e.Exception.Message;
        }
        e.Handled = true;
    }

    internal static void Log(string line)
    {
        try
        {
            var dir = Infinity.App.MainWindow.StateRoot; // DEVLAUNCH: a development build logs in its own launcher-dev
            Directory.CreateDirectory(dir);
            File.AppendAllText(Path.Combine(dir, "launcher.log"), $"{DateTimeOffset.Now:yyyy-MM-dd HH:mm:ss.fff zzz} {line}{Environment.NewLine}");
        }
        catch (IOException)
        {
            // Logging never breaks the shell.
        }
    }
}
