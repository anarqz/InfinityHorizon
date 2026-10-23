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
