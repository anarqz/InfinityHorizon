// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Threading;

namespace Infinity.App;

public partial class App : Application
{
    public App()
    {
        // A fault in one action never takes the launcher (and the play session it supervises) down:
        // it is written to launcher.log and the shell keeps running.
        DispatcherUnhandledException += OnDispatcherUnhandled;
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
