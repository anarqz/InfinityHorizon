// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using System.Text.Json;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Media;
using System.Windows.Media.Animation;
using System.Windows.Media.Imaging;
using System.Windows.Threading;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10 (the first frame: a splash screen only): a small window shown at process start, before the main window is built: the
/// Infinity logo, a spinner and "Starting Infinity Horizon…" in the launcher's locale, in the shell's colours. It lives on its own UI thread
/// (its own dispatcher ticks the spinner, so it turns while the window's thread builds the 3000-line main window), and closes once the main
/// window drew its first frame (<see cref="Window.ContentRendered"/>): the main window is already on screen then, so there is no gap.
/// Under Wine/Proton it draws the same in software; if its thread cannot start or the window cannot be built, the launcher starts without
/// it (logged). <c>FI_SPLASH=0</c> turns it off.
/// </summary>
internal static class StartSplash
{
    /// <summary>Where the splash is in its life (the harness checks it).</summary>
    internal enum Phase { NotShown, Starting, Shown, Closed, Failed }

    private static readonly Lock Gate = new();
    private static Dispatcher? _dispatcher;
    private static Window? _window;
    private static Thread? _thread;
    private static volatile Phase _phase = Phase.NotShown;
    private static long _shownAt, _closedAt;
    private static int _frames;

    internal static Phase State => _phase;

    /// <summary>The splash's thread (null before it starts); ended after <see cref="Close"/>.</summary>
    internal static Thread? SplashThread => _thread;

    /// <summary>How many frames the splash's own thread drew so far (its spinner ticks them, whatever the window's thread does).</summary>
    internal static int Frames => Volatile.Read(ref _frames);

    /// <summary>How long the splash was on screen (zero while it still is, or when it never was).</summary>
    internal static TimeSpan ShownFor => _shownAt > 0 && _closedAt > _shownAt ? System.Diagnostics.Stopwatch.GetElapsedTime(_shownAt, _closedAt) : TimeSpan.Zero;

    /// <summary>Why the splash failed (empty when it did not).</summary>
    internal static string Failure { get; private set; } = string.Empty;

    /// <summary>The line under the spinner in <paramref name="locale"/> (<c>splash.starting</c> of the window's strings; English if unreadable).</summary>
    internal static string TextFor(string locale)
    {
        const string fallback = "Starting Infinity Horizon…";
        try
        {
            var file = System.IO.Path.Combine(AppContext.BaseDirectory, "i18n-win", (locale is "pt-BR" ? "pt-BR" : "en") + ".json");
            if (!File.Exists(file)) return fallback;
            using var document = JsonDocument.Parse(File.ReadAllBytes(file));
            return document.RootElement.TryGetProperty("splash.starting", out var text) && text.GetString() is { Length: > 0 } value ? value : fallback;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException or JsonException) { return fallback; }
    }

    /// <summary>Shows the splash on its own thread; returns at once (the thread builds and shows it). Once per process.</summary>
    internal static void Show(string locale)
    {
        lock (Gate)
        {
            if (_phase != Phase.NotShown) return;
            if (Environment.GetEnvironmentVariable("FI_SPLASH") == "0") return;
            _phase = Phase.Starting;
        }
        var text = TextFor(locale);
        try
        {
            var thread = new Thread(() => Run(text)) { IsBackground = true, Name = "start-splash" };
            thread.SetApartmentState(ApartmentState.STA);
            _thread = thread;
            thread.Start();
        }
        catch (Exception e) when (e is OutOfMemoryException or ThreadStateException or InvalidOperationException)
        {
            Fail(e);
        }
    }

    /// <summary>Closes the splash (any thread; idempotent). The caller's window is on screen already.</summary>
    internal static void Close()
    {
        Dispatcher? dispatcher;
        lock (Gate)
        {
            if (_phase is Phase.NotShown or Phase.Closed or Phase.Failed) { if (_phase == Phase.NotShown) _phase = Phase.Closed; return; }
            dispatcher = _dispatcher;
            // still building: the thread closes it as soon as it would show (Run reads the phase)
            _phase = Phase.Closed;
            _closedAt = System.Diagnostics.Stopwatch.GetTimestamp();
        }
        if (dispatcher is null) return;
        try
        {
            dispatcher.BeginInvoke(() =>
            {
                try { _window?.Close(); }
                catch (InvalidOperationException) { }
                Dispatcher.CurrentDispatcher.BeginInvokeShutdown(DispatcherPriority.Background);
            });
        }
        catch (Exception e) when (e is InvalidOperationException or TaskCanceledException) { }
    }

    /// <summary>Waits for the splash's thread to end (the harness; never on the launcher's path).</summary>
    internal static bool WaitEnded(TimeSpan timeout) => _thread is not { } thread || thread.Join(timeout);

    private static void Fail(Exception e)
    {
        lock (Gate) { _phase = Phase.Failed; Failure = e.GetType().Name + ": " + e.Message; }
        App.Log("ui: start splash not shown (" + e.GetType().Name + "); the launcher starts without it");
    }

    /// <summary>Tests only: thrown while the splash is built on its thread (an error the old fixed list did not name).</summary>
    internal static Func<Exception>? BuildFaultForTests { get; set; }

    /// <summary>Tests only: a new splash may be shown again in this process (its last one ended).</summary>
    internal static void ResetForTests()
    {
        lock (Gate) { _phase = Phase.NotShown; Failure = string.Empty; _shownAt = _closedAt = 0; }
    }

    private static void Run(string text)
    {
        try
        {
            if (BuildFaultForTests?.Invoke() is { } fault) throw fault;
            var window = Build(text);
            lock (Gate)
            {
                if (_phase != Phase.Starting) return; // the main window rendered first: nothing to show
                _dispatcher = Dispatcher.CurrentDispatcher;
                _window = window;
            }
            CompositionTarget.Rendering += (_, _) => Interlocked.Increment(ref _frames);
            window.Show();
            lock (Gate)
            {
                if (_phase == Phase.Starting) { _phase = Phase.Shown; _shownAt = System.Diagnostics.Stopwatch.GetTimestamp(); }
            }
            Dispatcher.Run();
        }
        // review 3.1.1: any error on this background thread, not a fixed list (an unhandled one would end the whole launcher): logged, the
        // splash closed, the launcher starts without it. Only what nothing can carry on from goes through.
        catch (Exception e) when (e is not (StackOverflowException or AccessViolationException))
        {
            Fail(e);
            Window? built;
            lock (Gate) built = _window;
            try { built?.Close(); }
            catch (Exception closing) when (closing is not (StackOverflowException or AccessViolationException)) { App.Log("ui: start splash not closed (" + closing.GetType().Name + ")"); }
        }
        finally
        {
            lock (Gate) { _window = null; _dispatcher = null; }
        }
    }

    /// <summary>The splash window (on the splash's thread): the shell's palette from App.xaml, drawn without its resources.</summary>
    private static Window Build(string text)
    {
        static SolidColorBrush Brush(uint argb)
        {
            var brush = new SolidColorBrush(Color.FromArgb((byte)(argb >> 24), (byte)(argb >> 16), (byte)(argb >> 8), (byte)argb));
            brush.Freeze();
            return brush;
        }
        var background = Brush(0xFF0E0616); // NightBlack
        var border = Brush(0x29F9EAEE);     // InfinityBorder
        var line = Brush(0x24F9EAEE);       // SmLine
        var accent = Brush(0xFFF01864);     // NeonMagenta
        var foreground = Brush(0xFFF9EAEE); // SoftNeonWhite

        var logo = new BitmapImage();
        logo.BeginInit();
        logo.UriSource = new Uri("pack://application:,,,/Assets/logo.png");
        logo.DecodePixelWidth = 480;
        logo.CacheOption = BitmapCacheOption.OnLoad;
        logo.EndInit();
        logo.Freeze();

        // the ring and its quarter arc, turning on this thread's own dispatcher
        var ring = new Grid { Width = 40, Height = 40, Margin = new Thickness(0, 18, 0, 0), HorizontalAlignment = HorizontalAlignment.Center };
        ring.Children.Add(new System.Windows.Shapes.Ellipse { Stroke = line, StrokeThickness = 4 });
        var spin = new RotateTransform(0, 20, 20);
        ring.Children.Add(new System.Windows.Shapes.Path
        {
            Data = Geometry.Parse("M 20,2 A 18,18 0 0 1 38,20"), Stroke = accent, StrokeThickness = 4,
            StrokeStartLineCap = PenLineCap.Round, StrokeEndLineCap = PenLineCap.Round, RenderTransform = spin,
        });
        spin.BeginAnimation(RotateTransform.AngleProperty, new DoubleAnimation(0, 360, new Duration(TimeSpan.FromMilliseconds(900))) { RepeatBehavior = RepeatBehavior.Forever });

        var content = new StackPanel { VerticalAlignment = VerticalAlignment.Center, HorizontalAlignment = HorizontalAlignment.Center };
        content.Children.Add(new Image { Source = logo, Width = 240, HorizontalAlignment = HorizontalAlignment.Center });
        content.Children.Add(ring);
        content.Children.Add(new TextBlock
        {
            Text = text, Foreground = foreground, FontSize = 14, FontWeight = FontWeights.SemiBold, Margin = new Thickness(0, 14, 0, 0),
            HorizontalAlignment = HorizontalAlignment.Center,
        });
        var window = new Window
        {
            Title = "Infinity Horizon",
            WindowStyle = WindowStyle.None,
            ResizeMode = ResizeMode.NoResize,
            // no AllowsTransparency: a layered window is slow and drawn wrong under Wine; a plain borderless window is the same everywhere
            Width = 380, Height = 240,
            WindowStartupLocation = WindowStartupLocation.CenterScreen,
            ShowInTaskbar = true,
            ShowActivated = false,
            Background = background,
            BorderBrush = border,
            BorderThickness = new Thickness(1),
            Content = content,
        };
        System.Windows.Automation.AutomationProperties.SetName(window, text);
        return window;
    }
}
