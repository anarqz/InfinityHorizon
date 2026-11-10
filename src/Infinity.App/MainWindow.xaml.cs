// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.Windows;
using Infinity.Core;
using Infinity.Core.Api;
using Infinity.Core.Auth;

namespace Infinity.App;

public partial class MainWindow : Window
{
    private ShellViewModel _vm => (ShellViewModel)DataContext;

    private readonly GamepadNavigator _gamepad;

    public MainWindow()
    {
        // owner 2026-11-10: the start splash (App.OnStartup) closes once this window drew its first frame (it is on screen then: no gap);
        // first, so every path (the harness's too) closes it
        ContentRendered += (_, _) => StartSplash.Close();
        InitializeComponent();
        var cli = Environment.GetCommandLineArgs();
        var locale = LocaleOf(cli);
        var strings = Strings.Load(Path.Combine(AppContext.BaseDirectory, "i18n"), locale)
            .With(Path.Combine(AppContext.BaseDirectory, "i18n-win"));
        var arguments = cli.Skip(1).ToList();
        var state = StateRoot;
        var config = ConfigFor(arguments);
        var vm = new ShellViewModel(strings, config, state);

        DataContext = vm;
        _gamepad = new GamepadNavigator(this, () => vm.Playing);
        // OD-117: the set-up pane and its browser take the focus when they open, so the controller's D-pad and B act there.
        bool setupShown = false, browserShown = false;
        vm.PropertyChanged += (_, _) =>
        {
            if (vm.ShowExternalSetup == setupShown && vm.ShowBrowser == browserShown) return;
            var intoBrowser = vm.ShowBrowser && !browserShown;
            var intoPane = vm.ShowExternalSetup && (!setupShown || browserShown && !vm.ShowBrowser);
            setupShown = vm.ShowExternalSetup; browserShown = vm.ShowBrowser;
            if (intoBrowser) FocusBrowser();
            else if (intoPane) _ = Dispatcher.InvokeAsync(() => ExternalPane.MoveFocus(new System.Windows.Input.TraversalRequest(System.Windows.Input.FocusNavigationDirection.First)),
                System.Windows.Threading.DispatcherPriority.Loaded);
        };
        vm.PropertyChanged += (_, _) => { if (vm.OnlineServicePaused) { PasswordBox.Clear(); DeletionPasswordBox.Clear(); } };
        // Owner 2026-10-27: the Azimuth setup before sign-in takes the focus when it opens (controller-first: A chooses the folder).
        var azimuthFirstShown = false;
        vm.PropertyChanged += (_, _) =>
        {
            if (vm.ShowAzimuthFirst == azimuthFirstShown) return;
            azimuthFirstShown = vm.ShowAzimuthFirst;
            if (azimuthFirstShown) _ = Dispatcher.InvokeAsync(() => AzimuthFirstChoose.Focus(), System.Windows.Threading.DispatcherPriority.Loaded);
        };
        vm.UpdateRestartRequested += () => { App.ExitReason = "update restart"; Application.Current.Shutdown(); };
        // CR-L40: the sidebar's server status is read only while the window is shown (not hidden or minimized)
        void Shown() => vm.SetWindowShown(IsVisible && WindowState != WindowState.Minimized);
        IsVisibleChanged += (_, _) => Shown();
        StateChanged += (_, _) => Shown();
        Closing += (_, e) => { if (vm.ModOperationBusy) e.Cancel = true; };
        Closed += (_, _) => { PasswordBox.Clear(); DeletionPasswordBox.Clear(); vm.Dispose(); };
        PreviewKeyDown += OnBusyKey; // owner 2026-11-08: while the busy overlay is up no key reaches the page (Esc never cancels a write)
        PreviewMouseUp += OnBusyMouse;
        SetUpScreenLock(); // owner 2026-11-10: the lock blocks clicks and keys without greying the content
        KeyDown += OnSavesBackKey; // SAVE_MANAGER_UX §3: Esc goes back (a focused control that uses Esc handles it first)
        MouseUp += OnSavesMouseBack;
        var handled = false;
        ConfigureHarness(cli, vm, ref handled);
        if (handled) return;
        vm.ServerChangeRestartRequested += () => Dispatcher.BeginInvoke(RestartOnServerChoice);
        // owner 2026-11-10: a stall of the window's thread is logged (launcher.log), and this PC's saves are looked at again off the UI
        // thread when the window comes back to the front (the game or the player may have changed them meanwhile)
        vm.StartUiWatchdog(Dispatcher);
        Activated += async (_, _) => await vm.RefreshLocalFactsQuietlyAsync();
        // One launcher per state folder (launcher.lock).
        _ = vm.AcquireInstanceLockAsync();
        ContentRendered += async (_, _) => await vm.ConfirmUpdatedStartupAsync(cli);
        var azimuth = Array.IndexOf(cli, "--azimuth") is var i and >= 0 && i + 1 < cli.Length ? cli[i + 1] : Environment.GetEnvironmentVariable("FI_AZIMUTH");
        // OD-46: --azimuth / FI_AZIMUTH, else the saved choice, the launcher's folder, then a look around.
        Loaded += async (_, _) =>
        {
            if (await vm.RecoverInterruptedUpdateAsync(cli)) return;
            if (vm.DevelopmentMods && cli.Contains("--mods", StringComparer.Ordinal))
            {
                await vm.VerifyBuildOriginAsync();
                await vm.OpenDevelopmentModsAsync(azimuth);
                return;
            }
            await vm.StartupAsync(azimuth);
            // OD-136 / CR-L41: the Discord presence, the infinity-horizon:// registration (release builds) and a link this launch carries
            vm.StartPresence();
            vm.RegisterJoinLinks();
            if (Infinity.Core.Presence.JoinLink.FromArguments(arguments) is { } link) vm.ReceiveJoinLink(link);
        };
        vm.ActivateRequested += () =>
        {
            if (WindowState == WindowState.Minimized) WindowState = WindowState.Normal;
            Activate();
        };
    }

    /// <summary>The window's locale: <c>--locale en|pt-BR</c>, else the system's UI language (the start splash uses the same).</summary>
    internal static string LocaleOf(IReadOnlyList<string> cli)
    {
        var localeAt = cli.ToList().IndexOf("--locale");
        return localeAt >= 0 && localeAt + 1 < cli.Count && cli[localeAt + 1] is "en" or "pt-BR"
            ? cli[localeAt + 1] : Strings.LocaleFor(CultureInfo.CurrentUICulture);
    }

    /// <summary>The launcher's state root (each server's own folder below it, <see cref="ServerConfig.StateDirectory"/>).</summary>
    internal static string StateRoot => Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "ForzaInfinity", "launcher");

    /// <summary>CR-L39: the server the player chose on the screen, else the command line's (official in a release build).</summary>
    internal static ServerConfig ConfigFor(IReadOnlyList<string> arguments) =>
        ServerChoice.Apply(ServerConfig.From(arguments, Environment.GetEnvironmentVariable), StateRoot,
            explicitServer: arguments.Contains("--server") || Environment.GetEnvironmentVariable("FI_SERVER") is { Length: > 0 });

    partial void ConfigureHarness(string[] args, ShellViewModel vm, ref bool handled);

    private async void OnCheckLauncherUpdates(object sender, RoutedEventArgs e) => await _vm.CheckLauncherUpdatesInBackgroundAsync();

    // owner 2026-11-10: the bottom-right job toasts
    private void OnRetryJob(object sender, RoutedEventArgs e) => _vm.RetryJob((JobToast)((FrameworkElement)sender).DataContext);
    private void OnDismissJob(object sender, RoutedEventArgs e) => _vm.DismissJob((JobToast)((FrameworkElement)sender).DataContext);
    private void OnCancelJob(object sender, RoutedEventArgs e) => _vm.CancelJob((JobToast)((FrameworkElement)sender).DataContext);
    private async void OnCheckOnlineStatus(object sender, RoutedEventArgs e) => await _vm.CheckOnlineStatusAsync();
    private void OnOnlineSupport(object sender, RoutedEventArgs e) => _vm.OpenOnlineSupport();
    private async void OnConfirmInstall(object sender, RoutedEventArgs e) => await _vm.ConfirmInstallAsync();
    private void OnDismissGitHubAdvisory(object sender, RoutedEventArgs e) => _vm.DismissGitHubAdvisory();
    private void OnPlayOfflineFromStatus(object sender, RoutedEventArgs e) => _vm.PlayOfflineFromStatus();
    private void OnOpenOnlineStatus(object sender, RoutedEventArgs e) => _vm.OpenOnlineStatus();
    private void OnCloseOnlineStatus(object sender, RoutedEventArgs e) => _vm.CloseOnlineStatus();
    private async void OnApplyLauncherUpdate(object sender, RoutedEventArgs e) => await _vm.ApplyLauncherUpdateAsync();
    private void OnCancelLauncherUpdate(object sender, RoutedEventArgs e) => _vm.CancelLauncherUpdate();
    private void OnUpdateSupport(object sender, RoutedEventArgs e) => _vm.OpenUpdateSupport();
    private async void OnOpenLauncherUpdate(object sender, RoutedEventArgs e) => await _vm.LauncherUpdateBannerAsync();
    private void OnExitLauncherUpdate(object sender, RoutedEventArgs e) { App.ExitReason = "exit button (launcher update)"; Application.Current.Shutdown(); }

    private void OnOfficialReleases(object sender, RoutedEventArgs e) => OpenCommunityLink("https://github.com/anarqz/InfinityHorizon/releases");

    private void OnAbout(object sender, RoutedEventArgs e) => MessageBox.Show(this,
        $"Infinity Launcher {_vm.Version}\n\nBased on Infinity Launcher - https://github.com/anarqz/InfinityHorizon\n\n{_vm.S["legal.license"]}\n\n{_vm.S["legal.third_party"]}\n\n{_vm.S["legal.privacy"]}",
        _vm.S["legal.about"], MessageBoxButton.OK, MessageBoxImage.Information);

    private void OnCancelSignIn(object sender, RoutedEventArgs e)
    {
        PasswordBox.Clear();
        _vm.CancelSignIn();
    }

    // Clear the control immediately; the core owns credentials only on the verification step.
    private async void OnNativeSignIn(object sender, RoutedEventArgs e)
    {
        var password = PasswordBox.Password;
        PasswordBox.Clear();
        await _vm.NativeSignInAsync(password);
    }

    private async void OnNativeSignUp(object sender, RoutedEventArgs e)
    {
        var password = PasswordBox.Password;
        PasswordBox.Clear();
        await _vm.NativeSignUpAsync(password);
    }

    [System.Diagnostics.CodeAnalysis.SuppressMessage("Performance", "CA1822:Mark members as static", Justification = "XAML event handlers require an instance method.")]
    private void OnPasswordVisibility(object sender, DependencyPropertyChangedEventArgs e)
    {
        if (e.NewValue is false) ((System.Windows.Controls.PasswordBox)sender).Clear();
    }

    private async void OnConfirmed(object sender, RoutedEventArgs e) => await _vm.ConfirmVerificationAsync();

    private void OnCancelVerification(object sender, RoutedEventArgs e)
    {
        PasswordBox.Clear();
        _vm.CancelVerification();
    }

    private void OnNavAccount(object sender, RoutedEventArgs e) => _vm.OpenAccount();

    private async void OnRequestDeletion(object sender, RoutedEventArgs e)
    {
        var password = DeletionPasswordBox.Password;
        DeletionPasswordBox.Clear();
        await _vm.RequestAccountDeletionAsync(password);
    }

    private async void OnCancelDeletion(object sender, RoutedEventArgs e) => await _vm.CancelAccountDeletionAsync();

    private void OnPasswordKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key == System.Windows.Input.Key.Enter)
        {
            OnNativeSignIn(sender, e);
        }
    }

    private async void OnForgot(object sender, RoutedEventArgs e)
    {
        PasswordBox.Clear();
        await _vm.NativeForgotAsync();
    }

    private async void OnResend(object sender, RoutedEventArgs e) => await _vm.ResendVerificationAsync();
    private async void OnSubmitConfirmationCode(object sender, RoutedEventArgs e) => await _vm.SubmitConfirmationCodeAsync();
    private async void OnConfirmationCodeKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key == System.Windows.Input.Key.Enter && _vm.CanSubmitConfirmationCode) await _vm.SubmitConfirmationCodeAsync();
    }

    private async void OnCreateAccount(object sender, RoutedEventArgs e) => await _vm.CreateAccountAsync();

    // Owner 2026-11-08 (the PLAY revamp): the game tiles, the two profile buttons, the inline career start, the Profiles page
    private void OnSelectGameTile(object sender, RoutedEventArgs e) => _vm.SelectPlayGame((GameCard)((FrameworkElement)sender).DataContext);

    private async void OnPlayButton(object sender, RoutedEventArgs e) => await _vm.PlayButtonAsync((PlayButtonCard)((FrameworkElement)sender).DataContext);

    private async void OnPlayStartFresh(object sender, RoutedEventArgs e) => await _vm.PlayStartFreshAsync();

    private async void OnPlayStartFromProfile(object sender, RoutedEventArgs e) => await _vm.PlayStartFromProfileAsync();

    private async void OnPlayStartFromPc(object sender, RoutedEventArgs e) => await _vm.PlayStartFromPcAsync();

    private void OnClosePlayStart(object sender, RoutedEventArgs e) => _vm.ClosePlayStart();

    // NOOB_FROM_FILE (owner 2026-11-08): a Noob career from a save folder on this PC
    private void OnPlayStartFromFile(object sender, RoutedEventArgs e) => _vm.PlayStartFromFile();

    private async void OnConfirmFileStart(object sender, RoutedEventArgs e) => await _vm.ConfirmFileStartAsync();

    private void OnCancelFileStart(object sender, RoutedEventArgs e) => _vm.CancelFileStart();

    private void OnNavProfiles(object sender, RoutedEventArgs e) => _vm.OpenProfiles();

    // Owner 2026-11-08: the FRIENDS sidebar (header toggle)
    private void OnToggleFriends(object sender, RoutedEventArgs e) => _vm.ToggleFriends();

    private async void OnAddFriend(object sender, RoutedEventArgs e) => await _vm.AddFriendAsync();

    private async void OnFriendAddKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Enter) return;
        e.Handled = true;
        await _vm.AddFriendAsync();
    }

    private async void OnAcceptFriendRequest(object sender, RoutedEventArgs e) => await _vm.AcceptFriendRequestAsync((FriendRequestCard)((FrameworkElement)sender).DataContext);

    private async void OnDeclineFriendRequest(object sender, RoutedEventArgs e) => await _vm.DeclineFriendRequestAsync((FriendRequestCard)((FrameworkElement)sender).DataContext);

    private void OnAcceptFriendInvite(object sender, RoutedEventArgs e) => _vm.AcceptFriendInvite((FriendCard)((FrameworkElement)sender).DataContext);

    private async void OnDeclineFriendInvite(object sender, RoutedEventArgs e) => await _vm.DeclineFriendInviteAsync((FriendCard)((FrameworkElement)sender).DataContext);

    private async void OnConvertProfile(object sender, RoutedEventArgs e) => await _vm.AskConvertAsync(((ProfileRow)((FrameworkElement)sender).DataContext).Profile);

    private async void OnConfirmConvert(object sender, RoutedEventArgs e) => await _vm.ConfirmConvertAsync();

    private void OnCancelConvert(object sender, RoutedEventArgs e) => _vm.CancelConvert();

    /// <summary>CR-S61: the typed confirmation opens below the list and is scrolled into view.</summary>
    private async void OnDownloadCareer(object sender, RoutedEventArgs e) => await _vm.DownloadCareerAsync((ProfileRow)((FrameworkElement)sender).DataContext);

    private void OnDeleteWholeProfile(object sender, RoutedEventArgs e)
    {
        _vm.AskDeleteWholeProfile(((ProfileRow)((FrameworkElement)sender).DataContext).Profile);
        if (_vm.ShowDeleteProfile)
            Dispatcher.BeginInvoke(() => DeleteProfilePanel.BringIntoView(), System.Windows.Threading.DispatcherPriority.Loaded);
    }

    private void OnDeleteProfile(object sender, RoutedEventArgs e)
    {
        _vm.AskDeleteProfile(((ProfileRow)((FrameworkElement)sender).DataContext).Profile);
        if (_vm.ShowDeleteProfile)
            Dispatcher.BeginInvoke(() => DeleteProfilePanel.BringIntoView(), System.Windows.Threading.DispatcherPriority.Loaded);
    }

    // CR-S63: the account page's Discord card.
    private async void OnLinkDiscord(object sender, RoutedEventArgs e) => await _vm.LinkDiscordAsync();

    private async void OnCheckDiscord(object sender, RoutedEventArgs e) => await _vm.CheckDiscordNowAsync();

    private void OnCancelDiscordLink(object sender, RoutedEventArgs e) => _vm.CancelDiscordLink();

    private async void OnUnlinkDiscord(object sender, RoutedEventArgs e) => await _vm.UnlinkDiscordAsync();

    private async void OnAnnounceToggle(object sender, RoutedEventArgs e)
    {
        var box = (System.Windows.Controls.CheckBox)sender;
        await _vm.SetAnnounceAsync((Infinity.Core.Shell.AnnounceSwitch)box.DataContext, box.IsChecked == true);
    }

    private async void OnWebsiteToggle(object sender, RoutedEventArgs e)
    {
        var box = (System.Windows.Controls.CheckBox)sender;
        await _vm.SetWebsiteListedAsync((Infinity.Core.Shell.WebsiteSwitch)box.DataContext, box.IsChecked == true);
    }

    private async void OnConfirmDeleteProfile(object sender, RoutedEventArgs e) => await _vm.ConfirmDeleteProfileAsync();

    private void OnCancelDeleteProfile(object sender, RoutedEventArgs e) => _vm.CancelDeleteProfile();

    private async void OnConfirmSeed(object sender, RoutedEventArgs e) => await _vm.ConfirmSeedAsync();

    private async void OnDismissSeed(object sender, RoutedEventArgs e) => await _vm.DismissSeedAsync();

    private async void OnSeedScratch(object sender, RoutedEventArgs e) => await _vm.StartFromScratchAsync();

    private async void OnSeedCheckChoice(object sender, RoutedEventArgs e) => await _vm.CheckSeedChoiceAsync();

    private void OnSeedCloseChoice(object sender, RoutedEventArgs e) => _vm.CloseSeedChoice();

    private async void OnAbandonSeed(object sender, RoutedEventArgs e) => await _vm.AbandonSeedAsync();

    // OD-214: the Start panel of a profile without a career of the chosen game
    private async void OnStartCareerFresh(object sender, RoutedEventArgs e) => await _vm.StartCareerFreshAsync();

    private async void OnStartCareerExisting(object sender, RoutedEventArgs e) => await _vm.StartCareerFromExistingAsync();

    private async void OnStartCareerFromProfile(object sender, RoutedEventArgs e) => await _vm.StartCareerFromProfileAsync();

    /// <summary>The busy overlay's Cancel: only a sign-in waiting on the network (it writes nothing).</summary>
    private void OnBusyCancel(object sender, RoutedEventArgs e)
    {
        if (DataContext is ShellViewModel { CanCancelBusy: true } vm) vm.CancelBusy(); // a sign-in, or a Play still asking for its place
    }

    private void OnBusyKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        // Alt+F4 and the system keys stay the window's; every other key stops at the overlay (the Cancel button takes Enter/Space).
        // Owner 2026-11-10: a mod operation's lock too (the content is no longer disabled, so Tab and Enter must stop here).
        if (DataContext is not ShellViewModel vm || !Locked(vm) || e.Key == System.Windows.Input.Key.System) return;
        if (vm.ShowBusyOverlay && vm.CanCancelBusy && BusyOverlay.IsKeyboardFocusWithin && (e.Key == System.Windows.Input.Key.Enter || e.Key == System.Windows.Input.Key.Space)) return;
        // owner 2026-11-10 (3.1.1): the queue modal's own button takes Enter/Space (and Tab stays inside it)
        if (vm.ShowQueueModal && !vm.ShowBusyOverlay && QueueOverlay.IsKeyboardFocusWithin && e.Key is System.Windows.Input.Key.Enter or System.Windows.Input.Key.Space or System.Windows.Input.Key.Tab) return;
        e.Handled = true;
    }

    private void OnBusyMouse(object sender, System.Windows.Input.MouseButtonEventArgs e)
    {
        if (DataContext is ShellViewModel vm && Locked(vm) && e.ChangedButton is System.Windows.Input.MouseButton.XButton1 or System.Windows.Input.MouseButton.XButton2)
            e.Handled = true;
    }

    private void OnCancelCareerStart(object sender, RoutedEventArgs e) => _vm.CancelCareerStart();

    private void OnDeleteStartCareer(object sender, RoutedEventArgs e)
    {
        _vm.DeleteStartCareer();
        if (_vm.ShowDeleteProfile)
            Dispatcher.BeginInvoke(() => DeleteProfilePanel.BringIntoView(), System.Windows.Threading.DispatcherPriority.Loaded);
    }

    private void OnSeedDeleteCareer(object sender, RoutedEventArgs e)
    {
        _vm.DeleteCareerFromSeed();
        if (_vm.ShowDeleteProfile)
            Dispatcher.BeginInvoke(() => DeleteProfilePanel.BringIntoView(), System.Windows.Threading.DispatcherPriority.Loaded);
    }

    private async void OnCreateProfile(object sender, RoutedEventArgs e) => await _vm.CreateProfileAsync();

    // SAVE_MANAGER_UX (OD-207, OD-214): the save manager's screens
    private void OnSavesBack(object sender, RoutedEventArgs e) => _vm.SavesBack();
    private void OnSavesHome(object sender, RoutedEventArgs e) => _vm.SavesHome();
    private void OnSavesMenu(object sender, RoutedEventArgs e)
    {
        _vm.ToggleSavesMenu((SaveCard)((FrameworkElement)sender).DataContext);
        PlaceFloatingMenu((FrameworkElement)sender);
    }

    // owner 2026-11-08 (image 7): the open ⋯ floats by its button, inside the window; an outside click, the wheel, Esc or a resize closes it
    private FrameworkElement? _menuAnchor;
    private bool _menuResizeHooked;

    private void PlaceFloatingMenu(FrameworkElement anchor)
    {
        _menuAnchor = anchor;
        if (!_menuResizeHooked)
        {
            SizeChanged += (_, _) => _vm.CloseMenu();
            // the measured menu changes size once its items are drawn: place it again by the same button
            FloatingMenuPanel.SizeChanged += (_, _) => { if (_vm.ShowFloatingMenu) PositionFloatingMenu(); };
            _menuResizeHooked = true;
        }
        Dispatcher.BeginInvoke(() =>
        {
            // the cards are rebuilt when a menu opens: the anchor is the open menu's ⋯ button as drawn now
            if (!_vm.ShowFloatingMenu) return;
            UpdateLayout();
            if (!PositionFloatingMenu()) { _vm.CloseMenu(); return; }
            FloatingMenuPanel.MoveFocus(new System.Windows.Input.TraversalRequest(System.Windows.Input.FocusNavigationDirection.First));
        }, System.Windows.Threading.DispatcherPriority.Loaded);
    }

    /// <summary>
    /// Puts the open menu by its ⋯ button, in the menu layer's coordinates. The button is a sibling of the layer, not a child, so the
    /// transform goes through their common ancestor (TransformToVisual; TransformToAncestor threw here and left the menu at 0,0).
    /// </summary>
    private bool PositionFloatingMenu()
    {
        if (OpenMenuButton(this) is { } drawn) _menuAnchor = drawn;
        if (_menuAnchor is not { IsVisible: true } button || !button.IsDescendantOf(this) || !MenuLayer.IsVisible) return false;
        FloatingMenuPanel.Measure(new Size(double.PositiveInfinity, double.PositiveInfinity));
        var menu = FloatingMenuPanel.DesiredSize;
        var at = button.TransformToVisual(MenuLayer).TransformBounds(new Rect(button.RenderSize));
        var spot = Infinity.Core.Shell.MenuPlacement.Place(at.Left, at.Top, at.Width, at.Height, menu.Width, menu.Height, MenuLayer.ActualWidth, MenuLayer.ActualHeight);
        System.Windows.Controls.Canvas.SetLeft(FloatingMenuPanel, spot.Left);
        System.Windows.Controls.Canvas.SetTop(FloatingMenuPanel, spot.Top);
        return true;
    }

    /// <summary>The ⋯ button of the open menu's card or game row.</summary>
    private static System.Windows.Controls.Button? OpenMenuButton(DependencyObject root)
    {
        var count = System.Windows.Media.VisualTreeHelper.GetChildrenCount(root);
        for (var i = 0; i < count; i++)
        {
            var child = System.Windows.Media.VisualTreeHelper.GetChild(root, i);
            if (child is System.Windows.Controls.Button { Content: "⋯", DataContext: SaveCard { MenuOpen: true } or ProfileGameRow { MenuOpen: true } } button && button.IsVisible) return button;
            if (OpenMenuButton(child) is { } found) return found;
        }
        return null;
    }

    private void CloseFloatingMenu()
    {
        _vm.CloseMenu();
        if (_menuAnchor is { IsVisible: true } button) button.Focus();
        _menuAnchor = null;
    }

    private void OnMenuBackdrop(object sender, System.Windows.Input.MouseButtonEventArgs e) { CloseFloatingMenu(); e.Handled = true; }

    private void OnMenuBackdropWheel(object sender, System.Windows.Input.MouseWheelEventArgs e) => CloseFloatingMenu();

    private void OnMenuLayerKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Escape) return;
        e.Handled = true;
        CloseFloatingMenu();
    }
    private void OnSavesMenuItem(object sender, RoutedEventArgs e)
    {
        var element = (FrameworkElement)sender;
        if (element.Tag is SaveCard card && element.DataContext is MenuAction action) _vm.SavesMenu(card, action);
    }
    // owner 2026-11-08: each game row's own ⋯ (its career actions)
    private void OnGameRowMenu(object sender, RoutedEventArgs e)
    {
        _vm.ToggleGameRowMenu((ProfileGameRow)((FrameworkElement)sender).DataContext);
        PlaceFloatingMenu((FrameworkElement)sender);
    }
    private async void OnGameRowMenuItem(object sender, RoutedEventArgs e)
    {
        var element = (FrameworkElement)sender;
        if (element.Tag is ProfileGameRow row && element.DataContext is MenuAction action) await _vm.GameRowMenuAsync(row, action);
    }
    private void OnSavesCreate(object sender, RoutedEventArgs e) => _vm.OpenCreateProfile((SaveCard)((FrameworkElement)sender).DataContext);
    private void OnOpenManageSaves(object sender, RoutedEventArgs e) => _vm.OpenManageSaves();
    private void OnOpenSavesHelp(object sender, RoutedEventArgs e) => _vm.OpenSavesHelp();
    private async void OnSlotAction(object sender, RoutedEventArgs e)
    {
        var element = (FrameworkElement)sender;
        if (element.Tag is SlotCard slot && element.DataContext is MenuAction action) await _vm.SlotActionAsync(slot, action);
    }
    private void OnRestoreOriginal(object sender, RoutedEventArgs e) => _vm.OpenRestoreOriginal();
    private async void OnConfirmCopy(object sender, RoutedEventArgs e) => await _vm.ConfirmCopyAsync();
    private async void OnHistorySlot(object sender, RoutedEventArgs e)
    {
        if (Enum.TryParse<Infinity.Core.Shell.SaveSlot>(((FrameworkElement)sender).Tag as string, out var slot)) await _vm.ShowHistorySlotAsync(slot);
    }
    private async void OnHistoryRefresh(object sender, RoutedEventArgs e) => await _vm.RefreshHistoryAsync();
    private void OnSelectHistory(object sender, RoutedEventArgs e) => _vm.SelectHistory((HistoryRow)((FrameworkElement)sender).DataContext);
    private void OnHistoryOpenFolder(object sender, RoutedEventArgs e) => _vm.HistoryOpenFolder();
    private void OnHistoryCopy(object sender, RoutedEventArgs e) => _vm.HistoryCopyToAzimuth();
    private async void OnResolveKeepCloud(object sender, RoutedEventArgs e) => await _vm.ResolveKeepCloudAsync();
    private void OnResolveKeepPc(object sender, RoutedEventArgs e) => _vm.ResolveKeepPc();
    private void OnPlaySourceAsNoob(object sender, RoutedEventArgs e) => _vm.PlaySourceAsNoob();

    /// <summary>§3: Esc and the mouse's back button go one step back (the HARDCORE check first, then the save manager's screens).</summary>
    private void OnSavesBackKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Escape || e.Handled || DataContext is not ShellViewModel vm) return;
        if (vm.ShowSeedPanel) { vm.CloseSeedChoice(); e.Handled = true; }
        else if (vm.ShowSaveManager && vm.SavesCanGoBack) { vm.SavesBack(); e.Handled = true; }
    }

    private void OnSavesMouseBack(object sender, System.Windows.Input.MouseButtonEventArgs e)
    {
        if (e.ChangedButton != System.Windows.Input.MouseButton.XButton1 || DataContext is not ShellViewModel vm) return;
        if (vm.ShowSeedPanel) { vm.CloseSeedChoice(); e.Handled = true; }
        else if (vm.ShowSaveManager && vm.SavesCanGoBack) { vm.SavesBack(); e.Handled = true; }
    }

    private void OnSignOut(object sender, RoutedEventArgs e) => _vm.SignOut();

    private async void OnPlay(object sender, RoutedEventArgs e) => await _vm.PlayAsync();

    private void OnQuitGame(object sender, RoutedEventArgs e) => _vm.QuitGame();

    private void OnOpenSavePicker(object sender, RoutedEventArgs e) => _vm.OpenSavePicker();

    private async void OnPickSave(object sender, RoutedEventArgs e) => await _vm.PickSaveAsync((SaveChoice)((FrameworkElement)sender).DataContext);

    private void OnSkipSave(object sender, RoutedEventArgs e) => _vm.SkipSave();

    // OD-133: restore a career a fresh start moved aside on this PC
    private async void OnOpenCareerRestore(object sender, RoutedEventArgs e) => await _vm.OpenCareerRestoreAsync();

    private void OnPickCareerRestore(object sender, RoutedEventArgs e) => _vm.PickCareerRestore((CareerRestoreRow)((FrameworkElement)sender).DataContext);

    private async void OnConfirmCareerRestore(object sender, RoutedEventArgs e) => await _vm.ConfirmCareerRestoreAsync();

    private void OnBackCareerRestore(object sender, RoutedEventArgs e) => _vm.BackCareerRestore();

    private void OnCloseCareerRestore(object sender, RoutedEventArgs e) => _vm.CloseCareerRestore();

    private async void OnRestoreSetAside(object sender, RoutedEventArgs e) => await _vm.SetRestoreAsideAsync();

    private void OnRestoreKeep(object sender, RoutedEventArgs e) => _vm.KeepRestoredCareer();

    private void OnRestoreLater(object sender, RoutedEventArgs e) => _vm.CloseRestoreWaiting();

    private void OnFreshRefusedNewCareer(object sender, RoutedEventArgs e) => _vm.AnswerFreshStartRefused(Infinity.Core.Saves.FreshStartChoice.StartNewCareer);

    private void OnFreshRefusedNotNow(object sender, RoutedEventArgs e) => _vm.AnswerFreshStartRefused(Infinity.Core.Saves.FreshStartChoice.NotNow);

    private void OnNavHome(object sender, RoutedEventArgs e) => _vm.OpenHome();

    private void OnOpenSignIn(object sender, RoutedEventArgs e) => _vm.OpenSignIn();

    // CR-L39: the server choice on the welcome screen and a custom server's changed keys.
    private void OnServerChange(object sender, RoutedEventArgs e)
    {
        _vm.OpenServerChoice();
        // OD122-L9: focus lands on the chosen row (the D-pad moves through the list; the typed-address field, and with it the Steam
        // keyboard, opens only when the player goes to it)
        _ = Dispatcher.InvokeAsync(FocusServerPick, System.Windows.Threading.DispatcherPriority.Loaded);
    }

    /// <summary>The sign-in screen's "Change": back to the welcome screen with the picker open.</summary>
    private void OnSignInServerChange(object sender, RoutedEventArgs e)
    {
        OnWelcome(sender, e);
        OnServerChange(sender, e);
    }

    private void FocusServerPick()
    {
        if (ServerList.ItemContainerGenerator.ContainerFromItem(ServerList.SelectedItem) is System.Windows.Controls.ListBoxItem row) row.Focus();
        else ServerList.Focus();
    }

    private async void OnPickerRefresh(object sender, RoutedEventArgs e) => await _vm.RefreshPickersAsync();

    private void OnServerSave(object sender, RoutedEventArgs e) => _vm.SaveServerChoice();

    private void OnServerCancel(object sender, RoutedEventArgs e) => _vm.CancelServerChoice();

    private void OnTrustAccept(object sender, RoutedEventArgs e) => _vm.AcceptTrustChange();

    // Owner 2026-10-26: the save conflict's three answers.
    private void OnConflictKeepCloud(object sender, RoutedEventArgs e) => _vm.AnswerSaveConflict(Infinity.Core.Saves.ConflictChoice.KeepCloud);

    private void OnConflictKeepLocal(object sender, RoutedEventArgs e) => _vm.AnswerSaveConflict(Infinity.Core.Saves.ConflictChoice.KeepThisPc);

    private void OnConflictNotNow(object sender, RoutedEventArgs e) => _vm.AnswerSaveConflict(Infinity.Core.Saves.ConflictChoice.NotNow);

    private void OnTrustReject(object sender, RoutedEventArgs e) => _vm.RejectTrustChange();

    /// <summary>The launcher starts again on the saved server: a new process, then this one exits (its state folder is another).</summary>
    private static void RestartOnServerChoice()
    {
        try
        {
            // only the locale carries over: the server comes from the saved choice
            var arguments = Environment.GetCommandLineArgs().Skip(1).ToList();
            var start = new System.Diagnostics.ProcessStartInfo(Environment.ProcessPath ?? "infinity.exe") { UseShellExecute = false };
            var localeAt = arguments.IndexOf("--locale");
            if (localeAt >= 0 && localeAt + 1 < arguments.Count) { start.ArgumentList.Add("--locale"); start.ArgumentList.Add(arguments[localeAt + 1]); }
            System.Diagnostics.Process.Start(start)?.Dispose();
        }
        catch (Exception error) when (error is System.ComponentModel.Win32Exception or InvalidOperationException)
        {
            // the choice is saved; the next start uses it
        }
        App.ExitReason = "server change";
        Application.Current.Shutdown();
    }

    // OD-118: the welcome screen's Play offline link.
    private void OnPlayOfflineLink(object sender, RoutedEventArgs e) => _vm.PlayOffline();

    // OD-118: the Update online profiles panel after sign-in.
    private async void OnUploadProfiles(object sender, RoutedEventArgs e) => await _vm.UploadProfilesAsync();
    private void OnCloseProfileUpsert(object sender, RoutedEventArgs e) => _vm.CloseProfileUpsert();
    // OD-120: the one-time rename notice.
    private async void OnKeepRenamedName(object sender, RoutedEventArgs e) => await _vm.KeepRenamedNameAsync();
    private async void OnRenameProfile(object sender, RoutedEventArgs e) => await _vm.RenameProfileAsync();
    private void OnRenameLater(object sender, RoutedEventArgs e) => _vm.RenameLater();
    private void OnRenamePanelKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Escape) return;
        e.Handled = true; _vm.RenameLater();
    }
    private void OnUpsertPanelKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Escape) return;
        e.Handled = true; _vm.CloseProfileUpsert();
    }

    private void OnWelcome(object sender, RoutedEventArgs e)
    {
        PasswordBox.Clear();
        _vm.OpenWelcome();
    }

    private void OnLeaveQueue(object sender, RoutedEventArgs e) => _vm.LeaveQueue();

    private void OnNavBugReport(object sender, RoutedEventArgs e) => _vm.OpenBugReport();

    private void OnGitHubIssues(object sender, RoutedEventArgs e) => OpenCommunityLink("https://github.com/anarqz/InfinityHorizon/issues");

    private void OnDiscord(object sender, RoutedEventArgs e) => OpenCommunityLink("https://discord.gg/BqMVftFvXH");

    // OD-149: the anti-cheat isolation's dispute link (the community Discord)
    private void OnPoolDispute(object sender, RoutedEventArgs e) => OpenCommunityLink(Infinity.Core.Matchmaking.AntiCheatPool.DisputeLink);

    private static void OpenCommunityLink(string url) =>
        System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(url) { UseShellExecute = true })?.Dispose();

    private void OnNavMods(object sender, RoutedEventArgs e) => _vm.OpenMods();

    /// <summary>
    /// OD-119: the news column sits beside PLAY only when PLAY's own area is this wide (PLAY 600 + column 340 + gaps), measured on the
    /// content, so the lobby pane or a 1280x800 window moves it below PLAY instead of squeezing the button.
    /// </summary>
    private void OnNavNews(object sender, RoutedEventArgs e) => _vm.OpenNews();
    private async void OnNewsCard(object sender, RoutedEventArgs e)
    {
        await _vm.OpenNewsItemAsync((NewsCard)((FrameworkElement)sender).DataContext, fromHome: false); // owner 2026-11-08: news cards live only on the NEWS page
        // The pressed card collapsed with the list: the controller lands on Back (B / Escape also goes back).
        await Dispatcher.InvokeAsync(() => NewsBackButton.Focus(), System.Windows.Threading.DispatcherPriority.Loaded);
    }
    private void OnNewsBack(object sender, RoutedEventArgs e) => _vm.NewsBack();
    private void OnNewsKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Escape) return;
        e.Handled = true; _vm.NewsBack();
    }

    private async void OnLoadModCatalog(object sender, RoutedEventArgs e) => await _vm.LoadModCatalogAsync();
    // owner 2026-11-10: a card's click is awaited, and a failure is logged and said (never lost in a fire-and-forget task)
    private async void OnSelectMod(object sender, RoutedEventArgs e) { var card = (ModCard)((FrameworkElement)sender).DataContext; await _vm.GuardAsync("open a mod", () => _vm.SelectModAsync(card)); }
    private void OnSelectModsGame(object sender, RoutedEventArgs e) => _vm.SelectModsGame((GameCard)((FrameworkElement)sender).DataContext);
    private void OnModInformation(object sender, RoutedEventArgs e) => _vm.ShowModInformation((ModCard)((FrameworkElement)sender).DataContext);
    private async void OnInstallModSet(object sender, RoutedEventArgs e) => await _vm.InstallSetAsync((ModSetCard)((FrameworkElement)sender).DataContext);
    private async void OnUninstallModCard(object sender, RoutedEventArgs e) => await _vm.UninstallModCardAsync((ModCard)((FrameworkElement)sender).DataContext);
    private async void OnSelectedModOptions(object sender, RoutedEventArgs e) { if (_vm.SelectedMod is { } card) await _vm.GuardAsync("open the options", () => _vm.SelectModAsync(card)); }
    private void OnCloseModPanel(object sender, RoutedEventArgs e) => _vm.CloseModPanel();
    private void OnClearModSearch(object sender, RoutedEventArgs e) => _vm.ModSearch = "";
    // owner 2026-11-10 (build #3): the card's own button does its action (never the side pane); a failure is logged and said
    private async void OnActOnMod(object sender, RoutedEventArgs e) { var card = (ModCard)((FrameworkElement)sender).DataContext; await _vm.GuardAsync("act on a mod", () => _vm.ActOnModAsync(card)); }
    private async void OnToggleExtraOptionsMod(object sender, RoutedEventArgs e) => await _vm.ToggleExtraOptionsModAsync((ExtraOptionsModItem)((FrameworkElement)sender).DataContext);
    private async void OnDeleteExtraOptionsMod(object sender, RoutedEventArgs e) => await _vm.DeleteExtraOptionsModAsync((ExtraOptionsModItem)((FrameworkElement)sender).DataContext);
    private async void OnRecoverMods(object sender, RoutedEventArgs e) => await _vm.RecoverModsAsync();
    private async void OnResolveMods(object sender, RoutedEventArgs e) => await _vm.ResolveModRecoveryAsync();
    private async void OnRelinkMods(object sender, RoutedEventArgs e) => await _vm.RelinkModsAsync();
    private async void OnMoveModsLayout(object sender, RoutedEventArgs e) => await _vm.MoveModsLayoutAsync();
    private async void OnRetryModState(object sender, RoutedEventArgs e) => await _vm.RetryModStateAsync();
    private void OnOpenSetupForMods(object sender, RoutedEventArgs e) => _vm.OpenSetup();
    private async void OnConfirmModUpdate(object sender, RoutedEventArgs e) => await _vm.ConfirmModUpdateAsync();
    // owner 2026-11-10 (build #3): a card's Install or Update confirmed over the page
    private async void OnConfirmModCard(object sender, RoutedEventArgs e) => await _vm.GuardAsync("confirm a mod", _vm.ConfirmModCardAsync);
    private void OnCancelModConfirm(object sender, RoutedEventArgs e) => _vm.CancelModConfirm();
    private void OnModConfirmKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Escape) return;
        e.Handled = true; _vm.CancelModConfirm();
    }
    /// <summary>The confirmation opened: the keyboard and the controller land on Cancel (a second press never installs by accident).</summary>
    private void OnModConfirmVisible(object sender, DependencyPropertyChangedEventArgs e)
    {
        if (e.NewValue is true) _ = Dispatcher.InvokeAsync(() => ModConfirmCancelButton.Focus(), System.Windows.Threading.DispatcherPriority.Loaded);
    }
    private async void OnAcceptModFiles(object sender, RoutedEventArgs e) => await _vm.AcceptModFilesAsync();
    private async void OnDisableModKeeping(object sender, RoutedEventArgs e) => await _vm.DisableModKeepingFilesAsync();
    private async void OnRepairModSettings(object sender, RoutedEventArgs e) => await _vm.RepairModSettingsAsync();
    private async void OnUninstallMod(object sender, RoutedEventArgs e) => await _vm.UninstallModAsync();
    private async void OnPurgeMod(object sender, RoutedEventArgs e) => await _vm.PurgeModAsync();
    private async void OnUseCustomCatalog(object sender, RoutedEventArgs e) => await _vm.UseCustomCatalogAsync();

    // OD-117: the set-up pane of a mod made of the player's own files, and its in-launcher browser.
    private async void OnOpenExternalSetup(object sender, RoutedEventArgs e) { if (_vm.SelectedMod is { } card) await _vm.OpenExternalSetupAsync(card); }
    private void OnCloseExternalSetup(object sender, RoutedEventArgs e) => _vm.CloseExternalSetup();
    private async void OnInstallExternal(object sender, RoutedEventArgs e) => await _vm.InstallExternalAsync();

    private void OnBrowseExternal(object sender, RoutedEventArgs e) => _vm.StartBrowsing((ExternalSourceRow)((FrameworkElement)sender).DataContext);

    /// <summary>After the listing changes: the first entry takes the focus (else Up), never a collapsed button outside the pane.</summary>
    private void FocusBrowser() => _ = Dispatcher.InvokeAsync(() =>
    {
        if (BrowserList.Items.Count > 0 && BrowserList.ItemContainerGenerator.ContainerFromIndex(0) is DependencyObject first && FindButton(first) is { } button) button.Focus();
        else BrowserUpButton.Focus();
    }, System.Windows.Threading.DispatcherPriority.Loaded);

    private static System.Windows.Controls.Button? FindButton(DependencyObject node)
    {
        if (node is System.Windows.Controls.Button button) return button;
        for (var i = 0; i < System.Windows.Media.VisualTreeHelper.GetChildrenCount(node); i++)
            if (FindButton(System.Windows.Media.VisualTreeHelper.GetChild(node, i)) is { } found) return found;
        return null;
    }

    private void OnBrowsePlace(object sender, RoutedEventArgs e) { _vm.Browse(((BrowserPlace)((FrameworkElement)sender).DataContext).Path); FocusBrowser(); }
    private void OnBrowseDraft(object sender, RoutedEventArgs e) { _vm.BrowseDraft(); FocusBrowser(); }
    private void OnBrowseUp(object sender, RoutedEventArgs e) { _vm.BrowseUp(); FocusBrowser(); }
    private void OnStopBrowsing(object sender, RoutedEventArgs e) => _vm.StopBrowsing();
    private void OnPickerBackdrop(object sender, System.Windows.Input.MouseButtonEventArgs e) => _vm.StopBrowsing();
    private void OnPickerPaneKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Escape) return;
        e.Handled = true; _vm.StopBrowsing();
    }
    private async void OnPickBrowserFolder(object sender, RoutedEventArgs e) => await _vm.PickBrowserFolderAsync();
    private async void OnBrowserEntry(object sender, RoutedEventArgs e) { await _vm.OpenBrowserEntryAsync((BrowserEntry)((FrameworkElement)sender).DataContext); if (_vm.ShowBrowser) FocusBrowser(); }

    private void OnBrowserPathKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Enter) return;
        e.Handled = true; _vm.BrowseDraft(); FocusBrowser();
    }

    // Escape (the controller's B): back out of the browser first, then close the pane.
    private void OnExternalPaneKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Escape) return;
        e.Handled = true;
        if (_vm.ShowBrowser) _vm.StopBrowsing(); else _vm.CloseExternalSetup();
    }
    private async void OnUseOfficialCatalog(object sender, RoutedEventArgs e) => await _vm.UseOfficialCatalogAsync();



    private void OnSaveIni(object sender, RoutedEventArgs e) => _vm.SaveIni();


    // CR-S62: the moderation pane: opening focuses its first control, Escape (B) closes it.
    private void OnToggleModerationPane(object sender, RoutedEventArgs e)
    {
        _vm.ToggleModerationPane();
        if (_vm.ShowModerationPane)
            _ = Dispatcher.InvokeAsync(() =>
            {
                if (_vm.HasModerationPlayers) ModerationPlayerList.Focus();
                else ModerationLookupBox.Focus();
            }, System.Windows.Threading.DispatcherPriority.Input);
        else ModerationButton.Focus();
    }

    private void OnModerationPaneKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Escape || !_vm.ShowModerationPane) return;
        _vm.CloseModerationPane();
        ModerationButton.Focus();
        e.Handled = true;
    }

    private async void OnModerationRefresh(object sender, RoutedEventArgs e) => await _vm.RefreshModerationPlayersAsync();

    private async void OnModerationFind(object sender, RoutedEventArgs e) => await _vm.FindModerationTargetAsync();

    private async void OnModerationLookupKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Enter) return;
        e.Handled = true;
        await _vm.FindModerationTargetAsync();
    }

    private async void OnModerationApply(object sender, RoutedEventArgs e) => await _vm.ApplyModerationAsync();

    private void OnNavSetup(object sender, RoutedEventArgs e) => _vm.OpenSetup();

    /// <summary>Mods' "Use a different catalog…": Configure, scrolled to its catalog card.</summary>
    private void OnOpenCatalogSetting(object sender, RoutedEventArgs e)
    {
        _vm.OpenSetup();
        Dispatcher.InvokeAsync(() => { UpdateLayout(); CatalogCard.BringIntoView(); }, System.Windows.Threading.DispatcherPriority.Loaded);
    }

    private void OnInstallHook(object sender, RoutedEventArgs e) => _vm.InstallHook();

    private async void OnRecheckAzimuth(object sender, RoutedEventArgs e) => await _vm.RecheckAzimuthAsync();

    // Owner 2026-10-28: an Azimuth never opened blocks the launcher until it has been run once.
    private void OnLaunchAzimuthFirstRun(object sender, RoutedEventArgs e) => _vm.LaunchAzimuthForFirstRun();
    private void OnCheckAzimuthFirstRun(object sender, RoutedEventArgs e) => _vm.CheckAzimuthFirstRunAgain();
    private void OnLaunchAzimuthDirectly(object sender, RoutedEventArgs e) => _vm.LaunchAzimuthFromConfigure();

    // Owner 2026-10-28: every folder or file is chosen in the launcher's own picker (the right pane), never the Windows dialog.
    private void OnChooseAzimuth(object sender, RoutedEventArgs e) => _vm.ChooseAzimuthWithPicker();

    // Owner 2026-10-27: the Azimuth setup shown before sign-in.
    private void OnChooseAzimuthFirst(object sender, RoutedEventArgs e) => _vm.ChooseAzimuthFirstWithPicker();

    private async void OnRecheckAzimuthFirst(object sender, RoutedEventArgs e) => await _vm.RecheckAzimuthFirstAsync();

    private void OnAzimuthLater(object sender, RoutedEventArgs e) => _vm.SetUpAzimuthLater();

    // Azimuth's official page only (Almost There Games on itch.io).
    private void OnOpenAzimuthPage(object sender, RoutedEventArgs e)
    {
        var page = new Uri(Infinity.Core.Process.AzimuthSetup.OfficialPage);
        if (page.Scheme == Uri.UriSchemeHttps && page.Host == "almost-there-games.itch.io")
        {
            System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(page.AbsoluteUri) { UseShellExecute = true })?.Dispose();
        }
    }

    private void OnRevertIni(object sender, RoutedEventArgs e) => _vm.RevertIni();
    private void OnReviewCrash(object sender, RoutedEventArgs e) => _vm.ReviewCrash();
    private void OnLaterCrash(object sender, RoutedEventArgs e) => _vm.LaterCrash();
    private void OnDiscardCrash(object sender, RoutedEventArgs e) => _vm.DiscardCrash();
    private async void OnSendCrash(object sender, RoutedEventArgs e) => await _vm.SendCrashAsync();

    private void OnCopyCrash(object sender, RoutedEventArgs e)
    {
        if (_vm.CopyableCrashReport() is not { } report) return;
        try { Clipboard.SetText(report); _vm.CrashExportResult(true); }
        catch (System.Runtime.InteropServices.ExternalException) { _vm.CrashExportResult(false); }
    }

    private void OnSaveCrashReport(object sender, RoutedEventArgs e) => _vm.SaveCrashReportWithPicker();

    private void OnPrivateCrashSupport(object sender, RoutedEventArgs e)
    {
        if (_vm.CrashPrivateSupportUrl is { } url) OpenCommunityLink(url.AbsoluteUri);
    }

    private async void OnCheckIntegrity(object sender, RoutedEventArgs e) => await _vm.CheckIntegrityAsync();
    private void OnCancelIntegrity(object sender, RoutedEventArgs e) => _vm.CancelIntegrityCheck();
    private void OnChooseContent(object sender, RoutedEventArgs e) => _vm.ChooseContentWithPicker();
    private void OnCopyIntegrity(object sender, RoutedEventArgs e)
    {
        try { Clipboard.SetText(_vm.CopyableIntegrityReport()); _vm.IntegrityCopyResult(true); }
        catch (System.Runtime.InteropServices.ExternalException) { _vm.IntegrityCopyResult(false); }
    }

    private void OnSaveCrashDump(object sender, RoutedEventArgs e) => _vm.SaveCrashDumpWithPicker();

    // Only the catalog's https links are opened (the mods' official repositories).
    private void OnOpenLink(object sender, System.Windows.Navigation.RequestNavigateEventArgs e)
    {
        if (e.Uri.Scheme == Uri.UriSchemeHttps && e.Uri.Host == "github.com")
        {
            System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(e.Uri.AbsoluteUri) { UseShellExecute = true })?.Dispose();
        }
        e.Handled = true;
    }
}
