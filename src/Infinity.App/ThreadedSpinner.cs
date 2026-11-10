// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Animation;
using System.Windows.Threading;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10: "most of the time the spinner just appears stuck and gives the impression the app is locked". A WPF animation is
/// ticked by the dispatcher of the thread that owns it, so a spinner on the window's own thread stops whenever that thread is busy.
/// This one draws its ring on a dedicated UI thread (a <see cref="HostVisual"/> shown through a <see cref="VisualTarget"/>), whose own
/// dispatcher ticks the rotation: it keeps turning even while the window's thread is held. It animates only while <see cref="Spinning"/>
/// (nothing ticks while the overlay is hidden). Under Wine (software rendering, <see cref="Infinity.Core.Process.WineHost"/>) and if the
/// thread cannot start it falls back to the same ring animated on the window's thread.
/// </summary>
public sealed class ThreadedSpinner : FrameworkElement
{
    public static readonly DependencyProperty SpinningProperty = DependencyProperty.Register(nameof(Spinning), typeof(bool), typeof(ThreadedSpinner),
        new PropertyMetadata(false, (d, _) => ((ThreadedSpinner)d).Apply()));

    public static readonly DependencyProperty RingProperty = DependencyProperty.Register(nameof(Ring), typeof(Brush), typeof(ThreadedSpinner),
        new PropertyMetadata(Brushes.Gray));

    public static readonly DependencyProperty AccentProperty = DependencyProperty.Register(nameof(Accent), typeof(Brush), typeof(ThreadedSpinner),
        new PropertyMetadata(Brushes.Magenta));

    /// <summary>The window's thread is the spinner's (tests, Wine): set before the first spinner loads.</summary>
    internal static bool ForceWindowThread { get; set; } = Infinity.Core.Process.WineHost.Detected
        || Environment.GetEnvironmentVariable("FI_SPINNER") == "window";

    private HostVisual? _host;
    private Dispatcher? _worker;
    private RotateTransform? _rotation; // owned by _worker (or by this thread in the fallback)
    private DrawingVisual? _local;

    public ThreadedSpinner()
    {
        Loaded += (_, _) => Start();
        Unloaded += (_, _) => Stop();
    }

    /// <summary>
    /// False for the small toast spinners: background jobs leave the window's thread free by definition, and a toast is redrawn often,
    /// so it does not start a thread of its own.
    /// </summary>
    public bool OwnThread { get; set; } = true;

    public bool Spinning { get => (bool)GetValue(SpinningProperty); set => SetValue(SpinningProperty, value); }

    public Brush Ring { get => (Brush)GetValue(RingProperty); set => SetValue(RingProperty, value); }

    public Brush Accent { get => (Brush)GetValue(AccentProperty); set => SetValue(AccentProperty, value); }

    /// <summary>The ring runs on its own thread (false: the fallback on the window's thread).</summary>
    internal bool OnOwnThread => _worker is not null;

    protected override int VisualChildrenCount => _host is not null || _local is not null ? 1 : 0;

    protected override Visual GetVisualChild(int index) => (Visual?)_host ?? _local ?? throw new ArgumentOutOfRangeException(nameof(index));

    protected override Size MeasureOverride(Size availableSize) => new(Side, Side);

    private double Side => double.IsNaN(Width) ? 44 : Width;

    private void Start()
    {
        if (_host is not null || _local is not null) return;
        var side = Side;
        var ring = ColorOf(Ring);
        var accent = ColorOf(Accent);
        if (!ForceWindowThread && OwnThread)
        {
            try
            {
                var host = new HostVisual();
                using var ready = new ManualResetEventSlim(false);
                Dispatcher? worker = null;
                RotateTransform? rotation = null;
                var thread = new Thread(() =>
                {
                    var target = new VisualTarget(host);
                    var (visual, spin) = Draw(side, ring, accent);
                    target.RootVisual = visual;
                    worker = Dispatcher.CurrentDispatcher;
                    rotation = spin;
                    ready.Set();
                    Dispatcher.Run();
                    target.Dispose();
                }) { IsBackground = true, Name = "busy-spinner" };
                thread.SetApartmentState(ApartmentState.STA);
                thread.Start();
                if (!ready.Wait(TimeSpan.FromSeconds(2)) || worker is null) throw new InvalidOperationException("spinner thread did not start");
                _host = host; _worker = worker; _rotation = rotation;
                AddVisualChild(host);
                Apply();
                return;
            }
            catch (Exception e) when (e is InvalidOperationException or OutOfMemoryException or ThreadStateException)
            {
                System.Diagnostics.Trace.WriteLine("ui: spinner thread not started (" + e.GetType().Name + "); the window's thread draws it");
            }
        }
        var (local, localSpin) = Draw(side, ring, accent);
        _local = local; _rotation = localSpin;
        AddVisualChild(local);
        Apply();
    }

    private void Stop()
    {
        if (_host is { } host) { RemoveVisualChild(host); _host = null; }
        if (_local is { } local) { RemoveVisualChild(local); _local = null; }
        _worker?.BeginInvokeShutdown(DispatcherPriority.Normal);
        _worker = null; _rotation = null;
    }

    /// <summary>Starts or stops the rotation on the thread that owns it.</summary>
    private void Apply()
    {
        if (_rotation is not { } rotation) return;
        var spinning = Spinning;
        void Set()
        {
            if (!spinning) { rotation.BeginAnimation(RotateTransform.AngleProperty, null); return; }
            var turn = new DoubleAnimation(0, 360, new Duration(TimeSpan.FromMilliseconds(900))) { RepeatBehavior = RepeatBehavior.Forever };
            rotation.BeginAnimation(RotateTransform.AngleProperty, turn);
        }
        if (_worker is { } worker) worker.BeginInvoke(Set);
        else Set();
    }

    private static Color ColorOf(Brush brush) => brush is SolidColorBrush solid ? solid.Color : Colors.Gray;

    /// <summary>The ring (a full circle in the line colour, a quarter arc in the accent) turning around its centre.</summary>
    private static (DrawingVisual Visual, RotateTransform Spin) Draw(double side, Color ring, Color accent)
    {
        const double stroke = 4;
        var radius = (side - stroke) / 2;
        var center = new Point(side / 2, side / 2);
        var ringPen = new Pen(new SolidColorBrush(ring), stroke);
        ringPen.Freeze();
        var accentPen = new Pen(new SolidColorBrush(accent), stroke) { StartLineCap = PenLineCap.Round, EndLineCap = PenLineCap.Round };
        accentPen.Freeze();
        var arc = new StreamGeometry();
        using (var g = arc.Open())
        {
            g.BeginFigure(new Point(center.X, center.Y - radius), false, false);
            g.ArcTo(new Point(center.X + radius, center.Y), new Size(radius, radius), 0, false, SweepDirection.Clockwise, true, false);
        }
        arc.Freeze();
        var visual = new DrawingVisual();
        var spin = new RotateTransform(0, center.X, center.Y);
        visual.Transform = spin;
        using (var dc = visual.RenderOpen())
        {
            dc.DrawEllipse(null, ringPen, center, radius, radius);
            dc.DrawGeometry(null, accentPen, arc);
        }
        return (visual, spin);
    }
}
