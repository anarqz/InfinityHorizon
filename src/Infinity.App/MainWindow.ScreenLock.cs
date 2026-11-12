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

    /// <summary>The screen is locked: the centre overlay, the queue modal (owner 2026-11-10, 3.1.1) or a mod operation's lock is up.</summary>
    internal static bool Locked(ShellViewModel vm) => vm.ShowBusyOverlay || vm.ShowQueueModal || vm.ModOperationBusy;

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
        if (e.PropertyName is null or "" or nameof(ShellViewModel.ShowBusyOverlay) or nameof(ShellViewModel.ShowQueueModal) or nameof(ShellViewModel.ModOperationBusy) or nameof(ShellViewModel.ModContentEnabled))
        {
            var was = _locked;
            ApplyLock();
            // still locked, another overlay now (the Play's overlay and the queue modal take turns): the focus goes onto the one shown
            if (was && _locked && e.PropertyName is nameof(ShellViewModel.ShowBusyOverlay) or nameof(ShellViewModel.ShowQueueModal))
                Dispatcher.BeginInvoke(MoveFocusOffContent, DispatcherPriority.Loaded);
            // hotfix 3.1.2: a prompt that waits for the player just opened (the Play's overlay stepped aside): the keyboard goes onto it
            if (_lockVm is { } vm)
            {
                var prompt = vm.AwaitingPlayer && !_locked;
                if (prompt && !_promptOpen) Dispatcher.BeginInvoke(FocusOpenPrompt, DispatcherPriority.Loaded);
                _promptOpen = prompt;
            }
            // the Play's overlay offers Cancel (hotfix 3.1.2): reachable by keyboard too
            if (_lockVm is { ShowBusyOverlay: true, CanCancelBusy: true } && BusyOverlay.IsKeyboardFocused) BusyCancelButton.Focus();
        }
        else if (e.PropertyName is nameof(ShellViewModel.CanCancelBusy) && _lockVm is { ShowBusyOverlay: true, CanCancelBusy: true } && BusyOverlay.IsKeyboardFocused)
            Dispatcher.BeginInvoke(() => BusyCancelButton.Focus(), DispatcherPriority.Loaded);
    }

    /// <summary>A prompt waiting for the player is open and the screen is not locked (the keyboard was moved onto it once).</summary>
    private bool _promptOpen;

    /// <summary>The prompts that wait for the player (<see cref="ShellViewModel.AwaitingPlayer"/>) with their own panel: the visible one, or null.</summary>
    internal FrameworkElement? OpenPrompt() =>
        new FrameworkElement[] { SaveConflictPrompt, FreshStartPrompt, RestoreWaitingPrompt, CareerRestorePrompt, SeedPrompt, TrustChangePrompt, AzimuthFirstRunOverlay, ModConfirmOverlay, CopyConfirmOverlay, OldNoobNotice }
            .FirstOrDefault(p => p.IsVisible);

    /// <summary>Hotfix 3.1.2: the keyboard on the open prompt's first enabled button (its main choice), unless it is already inside it.</summary>
    private void FocusOpenPrompt()
    {
        if (_lockVm is not { AwaitingPlayer: true } vm || Locked(vm) || OpenPrompt() is not { } prompt || prompt.IsKeyboardFocusWithin) return;
        // owner 2026-11-12: the popups put the keyboard on their choice that changes nothing (their IsVisibleChanged handlers)
        if (ReferenceEquals(prompt, CopyConfirmOverlay)) { CopyCancelButton.Focus(); return; }
        if (ReferenceEquals(prompt, OldNoobNotice)) { if (OldNoobNoticeLater.IsVisible) OldNoobNoticeLater.Focus(); else OldNoobNoticeCloseButton.Focus(); return; }
        if (FirstButton(prompt) is { } button) button.Focus();
    }

    private static System.Windows.Controls.Button? FirstButton(DependencyObject root)
    {
        for (var i = 0; i < System.Windows.Media.VisualTreeHelper.GetChildrenCount(root); i++)
        {
            var child = System.Windows.Media.VisualTreeHelper.GetChild(root, i);
            if (child is UIElement { IsVisible: false }) continue;
            if (child is System.Windows.Controls.Button { IsEnabled: true, Focusable: true } button) return button;
            if (FirstButton(child) is { } found) return found;
        }
        return null;
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
        // hotfix 3.1.2: unlocked for a prompt waiting for the player: the keyboard goes onto it, not back to the control behind it
        else if (vm.AwaitingPlayer) { _focusBeforeLock = null; Dispatcher.BeginInvoke(FocusOpenPrompt, DispatcherPriority.Loaded); }
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
        // (the focus on an overlay that just closed, "Joining queue…" giving way to the queue modal, moves on too)
        if (!ShellContent.IsKeyboardFocusWithin && Keyboard.FocusedElement is not null and not UIElement { IsVisible: false }) return;
        if (vm.ShowBusyOverlay && BusyOverlay.IsVisible && BusyOverlay.Focus()) return;
        // the queue modal: its Leave queue button (else the modal itself), never the content behind it
        if (vm.ShowQueueModal && QueueOverlay.IsVisible && (QueueOverlay.IsKeyboardFocusWithin || QueueLeaveButton.Focus() || QueueOverlay.Focus())) return;
        if (ModProgressOverlay.IsVisible && ModProgressOverlay.Focus()) return;
        if (ShellContent.IsKeyboardFocusWithin) Keyboard.ClearFocus();
    }

    /// <summary>Harness seam: the content's lock as the window applies it (its hit testing, its look, where the focus is).</summary>
    internal bool IsBusyOverlayShown => BusyOverlay.IsVisible;

    internal (bool HitTestVisible, bool Enabled, bool FocusInContent) ContentLockState =>
        (ShellContent.IsHitTestVisible, ShellContent.IsEnabled, ShellContent.IsKeyboardFocusWithin);
}
