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
        InitializeComponent();
        var cli = Environment.GetCommandLineArgs();
        var localeAt = Array.IndexOf(cli, "--locale");
        var locale = localeAt >= 0 && localeAt + 1 < cli.Length && cli[localeAt + 1] is "en" or "pt-BR"
            ? cli[localeAt + 1] : Strings.LocaleFor(CultureInfo.CurrentUICulture);
        var strings = Strings.Load(Path.Combine(AppContext.BaseDirectory, "i18n"), locale)
            .With(Path.Combine(AppContext.BaseDirectory, "i18n-win"));
        var arguments = cli.Skip(1).ToList();
        var config = ServerConfig.From(arguments, Environment.GetEnvironmentVariable);
        var state = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "ForzaInfinity", "launcher");
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
        vm.UpdateRestartRequested += () => Application.Current.Shutdown();
        Closing += (_, e) => { if (vm.ModOperationBusy) e.Cancel = true; };
        Closed += (_, _) => { PasswordBox.Clear(); DeletionPasswordBox.Clear(); vm.Dispose(); };
        var handled = false;
        ConfigureHarness(cli, vm, ref handled);
        if (handled) return;
        // One launcher per state folder (launcher.lock): a second one cannot host a dedicated lobby (OD-109).
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
        };
    }

    partial void ConfigureHarness(string[] args, ShellViewModel vm, ref bool handled);

    private async void OnCheckLauncherUpdates(object sender, RoutedEventArgs e) => await _vm.CheckLauncherUpdatesAsync();
    private async void OnCheckOnlineStatus(object sender, RoutedEventArgs e) => await _vm.CheckOnlineStatusAsync();
    private void OnOnlineSupport(object sender, RoutedEventArgs e) => _vm.OpenOnlineSupport();
    private void OnDismissGitHubAdvisory(object sender, RoutedEventArgs e) => _vm.DismissGitHubAdvisory();
    private void OnPlayOfflineFromStatus(object sender, RoutedEventArgs e) => _vm.PlayOfflineFromStatus();
    private void OnOpenOnlineStatus(object sender, RoutedEventArgs e) => _vm.OpenOnlineStatus();
    private void OnCloseOnlineStatus(object sender, RoutedEventArgs e) => _vm.CloseOnlineStatus();
    private async void OnApplyLauncherUpdate(object sender, RoutedEventArgs e) => await _vm.ApplyLauncherUpdateAsync();
    private void OnCancelLauncherUpdate(object sender, RoutedEventArgs e) => _vm.CancelLauncherUpdate();
    private void OnUpdateSupport(object sender, RoutedEventArgs e) => _vm.OpenUpdateSupport();
    private async void OnOpenLauncherUpdate(object sender, RoutedEventArgs e)
    {
        if (_vm.LauncherReleaseUnavailable) await _vm.CheckLauncherUpdatesAsync();
        else _vm.OpenLauncherUpdate();
    }
    private void OnExitLauncherUpdate(object sender, RoutedEventArgs e) => Application.Current.Shutdown();

    private void OnOfficialReleases(object sender, RoutedEventArgs e) => OpenCommunityLink("https://github.com/anarqz/InfinityHorizon/releases");

    private void OnAbout(object sender, RoutedEventArgs e) => MessageBox.Show(this,
        $"Infinity Launcher {_vm.Version}\n\nBased on Infinity Launcher - https://github.com/anarqz/InfinityHorizon\n\n{_vm.S["legal.license"]}\n\n{_vm.S["legal.third_party"]}",
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

    private async void OnCreateAccount(object sender, RoutedEventArgs e) => await _vm.CreateAccountAsync();

    private void OnChooseGame(object sender, RoutedEventArgs e) => _vm.ChooseGame((GameInfo)((FrameworkElement)sender).DataContext);

    private async void OnConvertProfile(object sender, RoutedEventArgs e) => await _vm.AskConvertAsync(((ProfileRow)((FrameworkElement)sender).DataContext).Profile);

    private async void OnConfirmConvert(object sender, RoutedEventArgs e) => await _vm.ConfirmConvertAsync();

    private void OnCancelConvert(object sender, RoutedEventArgs e) => _vm.CancelConvert();

    private void OnChooseProfile(object sender, RoutedEventArgs e) => _vm.ChooseProfile(((ProfileRow)((FrameworkElement)sender).DataContext).Profile);

    private async void OnCreateProfile(object sender, RoutedEventArgs e) => await _vm.CreateProfileAsync();

    private void OnChangeGame(object sender, RoutedEventArgs e) => _vm.ChangeGame();

    private void OnChangeProfile(object sender, RoutedEventArgs e) => _vm.ChangeProfile();

    private void OnSignOut(object sender, RoutedEventArgs e) => _vm.SignOut();

    private async void OnPlay(object sender, RoutedEventArgs e) => await _vm.PlayAsync();

    private void OnQuitGame(object sender, RoutedEventArgs e) => _vm.QuitGame();

    private void OnOpenSavePicker(object sender, RoutedEventArgs e) => _vm.OpenSavePicker();

    private async void OnPickSave(object sender, RoutedEventArgs e) => await _vm.PickSaveAsync((SaveChoice)((FrameworkElement)sender).DataContext);

    private void OnSkipSave(object sender, RoutedEventArgs e) => _vm.SkipSave();

    private void OnNavHome(object sender, RoutedEventArgs e) => _vm.OpenHome();

    private void OnOpenSignIn(object sender, RoutedEventArgs e) => _vm.OpenSignIn();

    // OD-118: the welcome screen's Play offline link.
    private void OnPlayOfflineLink(object sender, RoutedEventArgs e) => _vm.PlayOffline();

    // OD-118: the Update online profiles panel after sign-in.
    private async void OnUploadProfiles(object sender, RoutedEventArgs e) => await _vm.UploadProfilesAsync();
    private void OnCloseProfileUpsert(object sender, RoutedEventArgs e) => _vm.CloseProfileUpsert();
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

    private static void OpenCommunityLink(string url) =>
        System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo(url) { UseShellExecute = true })?.Dispose();

    private void OnNavMods(object sender, RoutedEventArgs e) => _vm.OpenMods();
    private async void OnRequiredMod(object sender, RoutedEventArgs e) => await _vm.ActOnRequiredModAsync();

    private async void OnLoadModCatalog(object sender, RoutedEventArgs e) => await _vm.LoadModCatalogAsync();
    private void OnSelectMod(object sender, RoutedEventArgs e) => _vm.SelectMod((ModCard)((FrameworkElement)sender).DataContext);
    private void OnModInformation(object sender, RoutedEventArgs e) => _vm.ShowModInformation((ModCard)((FrameworkElement)sender).DataContext);
    private void OnSelectedModOptions(object sender, RoutedEventArgs e) { if (_vm.SelectedMod is { } card) _vm.SelectMod(card); }
    private void OnCloseModPanel(object sender, RoutedEventArgs e) => _vm.CloseModPanel();
    private void OnClearModSearch(object sender, RoutedEventArgs e) => _vm.ModSearch = "";
    private async void OnActOnMod(object sender, RoutedEventArgs e) => await _vm.ActOnModAsync((ModCard)((FrameworkElement)sender).DataContext);
    private async void OnRecoverMods(object sender, RoutedEventArgs e) => await _vm.RecoverModsAsync();
    private async void OnResolveMods(object sender, RoutedEventArgs e) => await _vm.ResolveModRecoveryAsync();
    private async void OnRelinkMods(object sender, RoutedEventArgs e) => await _vm.RelinkModsAsync();
    private async void OnConfirmModUpdate(object sender, RoutedEventArgs e) => await _vm.ConfirmModUpdateAsync();
    private async void OnConfirmCustomMod(object sender, RoutedEventArgs e) => await _vm.ConfirmCustomModAsync();
    private async void OnAcceptModFiles(object sender, RoutedEventArgs e) => await _vm.AcceptModFilesAsync();
    private async void OnDisableModKeeping(object sender, RoutedEventArgs e) => await _vm.DisableModKeepingFilesAsync();
    private async void OnRepairModSettings(object sender, RoutedEventArgs e) => await _vm.RepairModSettingsAsync();
    private async void OnUninstallMod(object sender, RoutedEventArgs e) => await _vm.UninstallModAsync();
    private void OnOpenCustomModsFolder(object sender, RoutedEventArgs e) => _vm.OpenCustomModsFolder();
    private async void OnAddModFromUrl(object sender, RoutedEventArgs e) => await _vm.AddModFromUrlAsync();

    private void OnToggleModUrl(object sender, RoutedEventArgs e)
    {
        _vm.ToggleModUrl();
        // The field opens focused, so the Steam keyboard (Game Mode / Big Picture only) opens with it.
        if (_vm.ShowModUrl) _ = Dispatcher.InvokeAsync(() => { ModUrlBox.Focus(); _gamepad.OpenSteamKeyboard(); }, System.Windows.Threading.DispatcherPriority.Input);
    }

    private void OnOpenModSchemaPage(object sender, RoutedEventArgs e) => OpenCommunityLink(ShellViewModel.ModSchemaPage);
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

    private async void OnSystemPickExternal(object sender, RoutedEventArgs e)
    {
        var row = (ExternalSourceRow)((FrameworkElement)sender).DataContext;
        string? picked = null;
        if (row.PicksFolder)
        {
            var dialog = new Microsoft.Win32.OpenFolderDialog { Title = row.Label };
            if (dialog.ShowDialog(this) == true) picked = dialog.FolderName;
        }
        else
        {
            var dialog = new Microsoft.Win32.OpenFileDialog { Title = row.Label, CheckFileExists = true };
            if (dialog.ShowDialog(this) == true) picked = dialog.FileName;
        }
        if (picked is not null) await _vm.PickWithSystemDialogAsync(row, picked);
    }

    private void OnBrowsePlace(object sender, RoutedEventArgs e) { _vm.Browse(((BrowserPlace)((FrameworkElement)sender).DataContext).Path); FocusBrowser(); }
    private void OnBrowseDraft(object sender, RoutedEventArgs e) { _vm.BrowseDraft(); FocusBrowser(); }
    private void OnBrowseUp(object sender, RoutedEventArgs e) { _vm.BrowseUp(); FocusBrowser(); }
    private void OnStopBrowsing(object sender, RoutedEventArgs e) => _vm.StopBrowsing();
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

    // Enter adds, Escape (the controller's B) closes the address field.
    private async void OnModUrlKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key == System.Windows.Input.Key.Escape) { _vm.ToggleModUrl(); e.Handled = true; }
        else if (e.Key == System.Windows.Input.Key.Enter) { e.Handled = true; await _vm.AddModFromUrlAsync(); }
    }



    private void OnSaveIni(object sender, RoutedEventArgs e) => _vm.SaveIni();


    // OD-109: the dedicated lobby pane. Opening focuses its first field (or Stop while running) for the
    // controller; Escape (the controller's B) closes it and returns focus to the globe.
    private void OnToggleLobbyPane(object sender, RoutedEventArgs e)
    {
        _vm.ToggleLobbyPane();
        if (_vm.ShowLobbyPane)
            _ = Dispatcher.InvokeAsync(() =>
            {
                if (_vm.DedicatedActive) LobbyStopButton.Focus();
                else LobbyNameBox.Focus();
            }, System.Windows.Threading.DispatcherPriority.Input);
        else LobbyButton.Focus();
    }

    private void OnLobbyPaneKey(object sender, System.Windows.Input.KeyEventArgs e)
    {
        if (e.Key != System.Windows.Input.Key.Escape || !_vm.ShowLobbyPane) return;
        _vm.CloseLobbyPane();
        LobbyButton.Focus();
        e.Handled = true;
    }

    private async void OnStartLobby(object sender, RoutedEventArgs e)
    {
        await _vm.StartLobbyAsync();
        // After the bindings have shown or hidden the buttons.
        _ = Dispatcher.BeginInvoke(() => { if (_vm.DedicatedActive && _vm.ShowLobbyPane) LobbyStopButton.Focus(); },
            System.Windows.Threading.DispatcherPriority.Loaded);
    }

    private async void OnStopLobby(object sender, RoutedEventArgs e)
    {
        await _vm.StopLobbyAsync();
        _ = Dispatcher.BeginInvoke(() => { if (_vm.ShowLobbyPane) LobbyCreateButton.Focus(); },
            System.Windows.Threading.DispatcherPriority.Loaded);
    }

    private void OnLobbyFewer(object sender, RoutedEventArgs e) => _vm.LobbyFewer();

    private void OnLobbyMore(object sender, RoutedEventArgs e) => _vm.LobbyMore();

    private void OnNavSetup(object sender, RoutedEventArgs e) => _vm.OpenSetup();

    private void OnInstallHook(object sender, RoutedEventArgs e) => _vm.InstallHook();

    private async void OnRecheckAzimuth(object sender, RoutedEventArgs e) => await _vm.RecheckAzimuthAsync();

    private void OnChooseAzimuth(object sender, RoutedEventArgs e)
    {
        var dialog = new Microsoft.Win32.OpenFolderDialog { Title = _vm.S["setup.choose_title"] };
        if (dialog.ShowDialog(this) == true)
        {
            _vm.ChooseAzimuthFolder(dialog.FolderName);
        }
    }

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

    private void OnSaveCrashReport(object sender, RoutedEventArgs e)
    {
        if (_vm.CopyableCrashReport() is not { } report) return;
        var dialog = new Microsoft.Win32.SaveFileDialog { FileName = "infinity-crash-report.json", DefaultExt = ".json", Filter = "JSON (*.json)|*.json", OverwritePrompt = true };
        if (dialog.ShowDialog(this) != true) return;
        try { File.WriteAllText(dialog.FileName, report, new System.Text.UTF8Encoding(false)); _vm.CrashExportResult(true); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException) { _vm.CrashExportResult(false); }
    }

    private void OnPrivateCrashSupport(object sender, RoutedEventArgs e)
    {
        if (_vm.CrashPrivateSupportUrl is { } url) OpenCommunityLink(url.AbsoluteUri);
    }

    private async void OnCheckIntegrity(object sender, RoutedEventArgs e) => await _vm.CheckIntegrityAsync();
    private void OnCancelIntegrity(object sender, RoutedEventArgs e) => _vm.CancelIntegrityCheck();
    private async void OnChooseContent(object sender, RoutedEventArgs e)
    {
        var dialog = new Microsoft.Win32.OpenFolderDialog { Title = _vm.S["integrity.choose_title"] };
        if (dialog.ShowDialog(this) == true) await _vm.ChooseContentFolderAsync(dialog.FolderName);
    }
    private void OnCopyIntegrity(object sender, RoutedEventArgs e)
    {
        try { Clipboard.SetText(_vm.CopyableIntegrityReport()); _vm.IntegrityCopyResult(true); }
        catch (System.Runtime.InteropServices.ExternalException) { _vm.IntegrityCopyResult(false); }
    }

    private async void OnSaveCrashDump(object sender, RoutedEventArgs e)
    {
        if (_vm.BeginManualCrashDumpExport() is not { } operation) return;
        bool? success = null;
        try
        {
            var dialog = new Microsoft.Win32.SaveFileDialog { FileName = "infinity-private-crash.dmp", DefaultExt = ".dmp", Filter = "Memory dump (*.dmp)|*.dmp", OverwritePrompt = true };
            if (dialog.ShowDialog(this) != true || !_vm.IsCurrentCrashExport(operation)) return;
            success = false;
            var bytes = await _vm.ReadCrashDumpForManualExportAsync(operation);
            if (bytes is null) return;
            await File.WriteAllBytesAsync(dialog.FileName, bytes, operation.Stop.Token);
            success = true;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or OperationCanceledException) { success = false; }
        finally { _vm.FinishManualCrashDumpExport(operation, success); }
    }

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
