// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.ComponentModel;
using System.Windows;
using System.Windows.Input;
using System.Windows.Threading;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10 (screen lock: block clicks without greying): while the busy overlay or a mod operation's lock is up, the content under
/// it keeps its look (no IsEnabled toggle: disabling the whole shell restyled every control, a visible grey flash and a long layout pass),
/// takes no clicks (<c>IsHitTestVisible</c> bound to <see cref="ShellViewModel.ModContentEnabled"/>; the busy overlay covers it) and no
/// keyboard: the focus leaves it for the overlay, cannot come back while locked (Tab, a controller's D-pad, a click), and every key but the
/// system ones stops at the window (<see cref="OnBusyKey"/>). The focus returns where it was when the lock ends.
/// </summary>
public partial class MainWindow
{
    private ShellViewModel? _lockVm;
    private bool _locked;
    private IInputElement? _focusBeforeLock;

    /// <summary>The screen is locked: the centre overlay or a mod operation's lock is up.</summary>
    internal static bool Locked(ShellViewModel vm) => vm.ShowBusyOverlay || vm.ModOperationBusy;

    private void SetUpScreenLock()
    {
        DataContextChanged += (_, _) => WatchLock(DataContext as ShellViewModel);
        WatchLock(DataContext as ShellViewModel);
        // the focus never enters the locked content (a controller's D-pad, Tab from the overlay, code that focuses a control)
        ShellContent.PreviewGotKeyboardFocus += (_, e) =>
        {
            if (_lockVm is { } vm && Locked(vm)) e.Handled = true;
        };
    }

    private void WatchLock(ShellViewModel? vm)
    {
        if (ReferenceEquals(vm, _lockVm)) return;
        if (_lockVm is { } old) old.PropertyChanged -= OnLockProperty;
        _lockVm = vm;
        _locked = false;
        if (vm is null) return;
        vm.PropertyChanged += OnLockProperty;
        ApplyLock();
    }

    private void OnLockProperty(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName is null or "" or nameof(ShellViewModel.ShowBusyOverlay) or nameof(ShellViewModel.ModOperationBusy) or nameof(ShellViewModel.ModContentEnabled))
            ApplyLock();
    }

    private void ApplyLock()
    {
        if (_lockVm is not { } vm) return;
        var locked = Locked(vm);
        if (locked == _locked) return;
        _locked = locked;
        if (locked)
        {
            if (ShellContent.IsKeyboardFocusWithin) _focusBeforeLock = Keyboard.FocusedElement;
            MoveFocusOffContent();
            // the overlay becomes visible with the bindings' update: the focus moves onto it once it is laid out
            Dispatcher.BeginInvoke(MoveFocusOffContent, DispatcherPriority.Loaded);
        }
        else if (_focusBeforeLock is UIElement { IsVisible: true, Focusable: true } before)
        {
            _focusBeforeLock = null;
            Dispatcher.BeginInvoke(() => { if (!_locked) before.Focus(); }, DispatcherPriority.Input);
        }
        else _focusBeforeLock = null;
    }

    /// <summary>The focus on the visible overlay (the busy one first), else nowhere: never on a control under the lock.</summary>
    private void MoveFocusOffContent()
    {
        if (_lockVm is not { } vm || !Locked(vm)) return;
        if (!ShellContent.IsKeyboardFocusWithin && Keyboard.FocusedElement is not null) return;
        if (vm.ShowBusyOverlay && BusyOverlay.IsVisible && BusyOverlay.Focus()) return;
        if (ModProgressOverlay.IsVisible && ModProgressOverlay.Focus()) return;
        if (ShellContent.IsKeyboardFocusWithin) Keyboard.ClearFocus();
    }

    /// <summary>Harness seam: the content's lock as the window applies it (its hit testing, its look, where the focus is).</summary>
    internal bool IsBusyOverlayShown => BusyOverlay.IsVisible;

    internal (bool HitTestVisible, bool Enabled, bool FocusInContent) ContentLockState =>
        (ShellContent.IsHitTestVisible, ShellContent.IsEnabled, ShellContent.IsKeyboardFocusWithin);
}
