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
        // A fault in one action never takes the launcher (and the play session it supervises) down:
        // it is written to launcher.log and the shell keeps running.
        DispatcherUnhandledException += OnDispatcherUnhandled;
        // OD-109: a Windows sign-out or shutdown closes the dedicated lobby (DELETE, then the gate leave), bounded.
        SessionEnding += (_, _) => (Current?.MainWindow?.DataContext as ShellViewModel)?.StopDedicatedForExit(TimeSpan.FromSeconds(4));
        TaskScheduler.UnobservedTaskException += (_, e) =>
        {
            Log("unobserved task: " + e.Exception);
            e.SetObserved();
        };
        // T4b: why the launcher ended, so a log never just stops (a fatal fault, or which way it was closed). No personal data: the
        // reason and the exit code only; a fault's exception as the other handlers log it.
        AppDomain.CurrentDomain.UnhandledException += (_, e) => Log("fatal: " + e.ExceptionObject + (e.IsTerminating ? " (terminating)" : ""));
        Exit += (_, e) => Log("exit: " + ExitReason + ", code " + e.ApplicationExitCode.ToString(System.Globalization.CultureInfo.InvariantCulture));
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
        base.OnStartup(e);
    }

    /// <summary>How the launcher is ending, for the exit line in launcher.log; the window close is the default.</summary>
    internal static string ExitReason { get; set; } = "window closed";

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
            var dir = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "ForzaInfinity", "launcher");
            Directory.CreateDirectory(dir);
            File.AppendAllText(Path.Combine(dir, "launcher.log"), $"{DateTimeOffset.Now:yyyy-MM-dd HH:mm:ss.fff zzz} {line}{Environment.NewLine}");
        }
        catch (IOException)
        {
            // Logging never breaks the shell.
        }
    }
}
