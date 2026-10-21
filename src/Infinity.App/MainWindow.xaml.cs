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
        Closed += (_, _) => { PasswordBox.Clear(); DeletionPasswordBox.Clear(); vm.Dispose(); };
        var handled = false;
        ConfigureHarness(cli, vm, ref handled);
        if (handled) return;
        Loaded += async (_, _) => await vm.VerifyBuildOriginAsync();
        var azimuth = Array.IndexOf(cli, "--azimuth") is var i and >= 0 && i + 1 < cli.Length ? cli[i + 1] : Environment.GetEnvironmentVariable("FI_AZIMUTH");
        // OD-46: --azimuth / FI_AZIMUTH, else the saved choice, the launcher's folder, then a look around.
        if (azimuth is not null)
        {
            vm.Install = Infinity.Core.Process.AzimuthInstall.Detect(azimuth);
        }
        else
        {
            Loaded += async (_, _) => await vm.FindAzimuthAsync(null, openSetupWhenNeeded: true);
        }
        // the server's state at start, whatever found Azimuth (the top bar never says OFFLINE on a guess)
        Loaded += async (_, _) => await vm.ProbeServerAsync();
    }

    partial void ConfigureHarness(string[] args, ShellViewModel vm, ref bool handled);

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

    private async void OnInstallEo(object sender, RoutedEventArgs e) => await _vm.InstallEoAsync();

    private async void OnCheckMods(object sender, RoutedEventArgs e) => await _vm.CheckModsAsync();

    private void OnSaveIni(object sender, RoutedEventArgs e) => _vm.SaveIni();

    private void OnToggleModOptions(object sender, RoutedEventArgs e) => _vm.ToggleModOptions();

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
    private void OnChooseContent(object sender, RoutedEventArgs e)
    {
        var dialog = new Microsoft.Win32.OpenFolderDialog { Title = _vm.S["integrity.choose_title"] };
        if (dialog.ShowDialog(this) == true) _vm.ChooseContentFolder(dialog.FolderName);
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
