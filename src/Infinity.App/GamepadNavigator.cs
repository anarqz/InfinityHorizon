// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Automation.Peers;
using System.Windows.Automation.Provider;
using System.Windows.Controls;
using System.Windows.Controls.Primitives;
using System.Windows.Documents;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Threading;

namespace Infinity.App;

/// <summary>
/// Controller navigation (Steam Deck, any XInput pad; Steam Input presents one to Proton): the
/// D-pad or left stick moves the keyboard focus, A presses, B is Escape, LB/RB step backwards and
/// forwards, the right stick scrolls. A highlight follows the focus while the controller is in use. Text boxes open the
/// Steam on-screen keyboard in Steam's Game Mode.
/// </summary>
internal sealed class GamepadNavigator
{
    private const ushort Up = 0x0001, Down = 0x0002, Left = 0x0004, Right = 0x0008;
    private const ushort Lb = 0x0100, Rb = 0x0200, A = 0x1000, B = 0x2000;
    private const short Stick = 16000, ScrollDeadZone = 8000;
    private const double ScrollSpeed = 22; // pixels per poll at full tilt
    private static readonly TimeSpan FirstRepeat = TimeSpan.FromMilliseconds(400), NextRepeat = TimeSpan.FromMilliseconds(110);

    /// <summary>Steam's Game Mode (gamescope) or Big Picture: no physical keyboard to expect.</summary>
    public static bool SteamGamepadUi => Environment.GetEnvironmentVariable("SteamGamepadUI") == "1"
        || Environment.GetEnvironmentVariable("SteamDeck") == "1";

    private readonly Window _window;
    private readonly Func<bool> _paused;
    private readonly DispatcherTimer _timer;
    private ushort _held;
    private ushort _repeating;
    private DateTime _repeatAt;
    private bool _active;
    private FocusAdorner? _adorner;
    private DateTime _keyboardOpened;

    public GamepadNavigator(Window window, Func<bool> paused)
    {
        _window = window;
        _paused = paused;
        _timer = new DispatcherTimer(TimeSpan.FromMilliseconds(16), DispatcherPriority.Input, (_, _) => Poll(), window.Dispatcher);
        window.Loaded += (_, _) => _timer.Start();
        window.Closed += (_, _) => _timer.Stop();
        window.PreviewMouseMove += (_, _) => { if (_active) { _active = false; Highlight(null); } };
        window.PreviewKeyDown += (_, _) => { if (_active) { _active = false; Highlight(null); } };
        window.AddHandler(UIElement.GotKeyboardFocusEvent, new KeyboardFocusChangedEventHandler((_, e) =>
        {
            if (_active) Highlight(e.NewFocus as FrameworkElement);
        }), true);
        // A tap or click on a text field opens the on-screen keyboard where there is no physical one.
        window.AddHandler(UIElement.PreviewMouseLeftButtonUpEvent, new MouseButtonEventHandler((_, e) =>
        {
            if (TextField(e.OriginalSource as DependencyObject) is not null) OpenSteamKeyboard();
        }), true);
    }

    private void Poll()
    {
        if (!_window.IsActive || _paused() || !Read(out var buttons, out var scroll))
        {
            _held = _repeating = 0;
            return;
        }
        var pressed = (ushort)(buttons & ~_held);
        _held = buttons;
        foreach (var direction in new[] { Up, Down, Left, Right })
        {
            if ((pressed & direction) != 0)
            {
                Move(direction);
                _repeating = direction;
                _repeatAt = DateTime.UtcNow + FirstRepeat;
            }
        }
        if (_repeating != 0)
        {
            if ((buttons & _repeating) == 0) _repeating = 0;
            else if (DateTime.UtcNow >= _repeatAt)
            {
                Move(_repeating);
                _repeatAt = DateTime.UtcNow + NextRepeat;
            }
        }
        if ((pressed & A) != 0) Press();
        if ((pressed & B) != 0) Escape();
        if ((pressed & Lb) != 0) Step(FocusNavigationDirection.Previous);
        if ((pressed & Rb) != 0) Step(FocusNavigationDirection.Next);
        if (Math.Abs((int)scroll) > ScrollDeadZone) Scroll(scroll);
    }

    /// <summary>The first connected controller's buttons, the left stick folded into the D-pad.</summary>
    private static bool Read(out ushort buttons, out short scroll)
    {
        buttons = 0;
        scroll = 0;
        for (uint pad = 0; pad < 4; pad++)
        {
            XInputState state;
            try
            {
                if (XInputGetState(pad, out state) != 0) continue;
            }
            catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
            {
                return false;
            }
            var g = state.Gamepad;
            buttons = g.Buttons;
            scroll = g.ThumbRY;
            if (g.ThumbLY > Stick) buttons |= Up;
            if (g.ThumbLY < -Stick) buttons |= Down;
            if (g.ThumbLX < -Stick) buttons |= Left;
            if (g.ThumbLX > Stick) buttons |= Right;
            return true;
        }
        return false;
    }

    private IInputElement? Focused()
    {
        var focused = Keyboard.FocusedElement as DependencyObject;
        return focused is not null && Window.GetWindow(focused) == _window ? (IInputElement)focused : null;
    }

    /// <summary>The first press shows where the focus is (or puts it on the first control) instead of moving.</summary>
    private bool Wake()
    {
        if (Focused() is FrameworkElement current && current != _window && current.IsVisible)
        {
            if (_active) return false;
            _active = true;
            Highlight(current);
            return true;
        }
        _active = true;
        _window.MoveFocus(new TraversalRequest(FocusNavigationDirection.First));
        SkipContainers(FocusNavigationDirection.Next);
        Highlight(Keyboard.FocusedElement as FrameworkElement);
        return true;
    }

    private void Move(ushort direction)
    {
        if (Wake()) return;
        var way = direction switch
        {
            Up => FocusNavigationDirection.Up,
            Down => FocusNavigationDirection.Down,
            Left => FocusNavigationDirection.Left,
            _ => FocusNavigationDirection.Right,
        };
        if (Focused() is UIElement element && !element.MoveFocus(new TraversalRequest(way)))
        {
            way = way is FocusNavigationDirection.Up or FocusNavigationDirection.Left ? FocusNavigationDirection.Previous : FocusNavigationDirection.Next;
            element.MoveFocus(new TraversalRequest(way));
        }
        SkipContainers(way);
    }

    private void Step(FocusNavigationDirection way)
    {
        if (Wake()) return;
        (Focused() as UIElement)?.MoveFocus(new TraversalRequest(way));
        SkipContainers(way);
    }

    /// <summary>Scroll areas and plain item lists can hold focus but do nothing with it: move past them.</summary>
    private void SkipContainers(FocusNavigationDirection way)
    {
        for (var i = 0; i < 8 && Focused() is ScrollViewer or ItemsControl and not Selector and not MenuBase and not TreeView; i++)
        {
            if (!((UIElement)Focused()!).MoveFocus(new TraversalRequest(way))) break;
        }
    }

    /// <summary>The right stick scrolls the area around the focus, else the visible page's.</summary>
    private void Scroll(short tilt)
    {
        var viewer = Ancestor<ScrollViewer>(Focused() as DependencyObject) ?? LargestVisible<ScrollViewer>(_window);
        viewer?.ScrollToVerticalOffset(viewer.VerticalOffset - tilt / 32767.0 * ScrollSpeed);
    }

    private static T? Ancestor<T>(DependencyObject? node) where T : DependencyObject
    {
        for (; node is not null; node = node is Visual ? VisualTreeHelper.GetParent(node) : LogicalTreeHelper.GetParent(node))
        {
            if (node is T found) return found;
        }
        return null;
    }

    private static T? LargestVisible<T>(DependencyObject root) where T : FrameworkElement
    {
        T? best = null;
        var pending = new Stack<DependencyObject>([root]);
        while (pending.Count > 0)
        {
            var node = pending.Pop();
            if (node is UIElement { IsVisible: false }) continue;
            if (node is T found && (best is null || found.ActualHeight * found.ActualWidth > best.ActualHeight * best.ActualWidth)) best = found;
            for (var i = 0; i < VisualTreeHelper.GetChildrenCount(node); i++) pending.Push(VisualTreeHelper.GetChild(node, i));
        }
        return best;
    }

    private void Press()
    {
        if (Wake() && Focused() is not TextBox and not PasswordBox) return;
        switch (Focused())
        {
            case TextBox or PasswordBox:
                OpenSteamKeyboard();
                break;
            case ComboBox combo:
                combo.IsDropDownOpen = !combo.IsDropDownOpen;
                break;
            case ToggleButton toggle when UIElementAutomationPeer.CreatePeerForElement(toggle)?.GetPattern(PatternInterface.Toggle) is IToggleProvider t:
                t.Toggle();
                break;
            case ButtonBase button when button.IsEnabled && UIElementAutomationPeer.CreatePeerForElement(button)?.GetPattern(PatternInterface.Invoke) is IInvokeProvider invoke:
                invoke.Invoke();
                break;
            case ListBoxItem item:
                item.IsSelected = true;
                break;
            case UIElement element:
                Key(element, System.Windows.Input.Key.Enter);
                break;
        }
    }

    private void Escape()
    {
        if (Wake()) return;
        if (Focused() is UIElement element) Key(element, System.Windows.Input.Key.Escape);
    }

    private static void Key(UIElement target, Key key)
    {
        if (PresentationSource.FromVisual(target) is not { } source) return;
        target.RaiseEvent(new KeyEventArgs(Keyboard.PrimaryDevice, source, 0, key) { RoutedEvent = Keyboard.PreviewKeyDownEvent });
        target.RaiseEvent(new KeyEventArgs(Keyboard.PrimaryDevice, source, 0, key) { RoutedEvent = Keyboard.KeyDownEvent });
    }

    private static Control? TextField(DependencyObject? source)
    {
        for (var node = source; node is not null; node = node is Visual or System.Windows.Media.Media3D.Visual3D ? VisualTreeHelper.GetParent(node) : LogicalTreeHelper.GetParent(node))
        {
            if (node is TextBox or PasswordBox) return (Control)node;
        }
        return null;
    }

    /// <summary>
    /// Steam's on-screen keyboard: Proton hands steam:// links to the Steam client (checked on a Deck in
    /// Game Mode). Typed text arrives as ordinary key presses in the focused field. Also called when a
    /// field opens already focused (Add from URL), where no tap or A press would open it.
    /// </summary>
    internal void OpenSteamKeyboard()
    {
        if (!SteamGamepadUi || DateTime.UtcNow - _keyboardOpened < TimeSpan.FromSeconds(1)) return;
        _keyboardOpened = DateTime.UtcNow;
        try
        {
            Process.Start(new ProcessStartInfo("steam://open/keyboard") { UseShellExecute = true })?.Dispose();
        }
        catch (Exception e) when (e is System.ComponentModel.Win32Exception or InvalidOperationException)
        {
            App.Log("gamepad: steam keyboard: " + e.Message);
        }
    }

    private void Highlight(FrameworkElement? element)
    {
        if (_adorner is not null)
        {
            AdornerLayer.GetAdornerLayer(_adorner.AdornedElement)?.Remove(_adorner);
            _adorner = null;
        }
        if (element is null || element == _window || AdornerLayer.GetAdornerLayer(element) is not { } layer) return;
        _adorner = new FocusAdorner(element, (Brush)_window.FindResource("InfinityAccent"));
        layer.Add(_adorner);
        element.BringIntoView();
    }

    private sealed class FocusAdorner(UIElement element, Brush brush) : Adorner(element)
    {
        private readonly Pen _pen = new(brush, 2);

        protected override void OnRender(DrawingContext drawing)
        {
            var size = AdornedElement.RenderSize;
            drawing.DrawRoundedRectangle(null, _pen, new Rect(-3, -3, size.Width + 6, size.Height + 6), 6, 6);
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct XInputGamepad
    {
        public ushort Buttons;
        public byte LeftTrigger, RightTrigger;
        public short ThumbLX, ThumbLY, ThumbRX, ThumbRY;
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct XInputState
    {
        public uint PacketNumber;
        public XInputGamepad Gamepad;
    }

    [DllImport("xinput1_4.dll")]
    private static extern uint XInputGetState(uint userIndex, out XInputState state);
}
