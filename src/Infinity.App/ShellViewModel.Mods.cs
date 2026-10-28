// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Globalization;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>One manifest option in the form. Numbers show in the player's culture; locked options are read-only with the manifest's reason.</summary>
public sealed class ManifestOptionItem(JsonElement definition, JsonElement value, string locale, Action changed) : INotifyPropertyChanged
{
    private readonly CultureInfo _culture = CultureInfo.GetCultureInfo(locale);
    public string Id => definition.GetProperty("id").GetString()!;
    public string Label => Localized("label") ?? Id;
    public string Help => Localized("description") ?? "";
    public bool IsBoolean => definition.GetProperty("type").GetString() == "boolean";
    public bool IsChoice => definition.GetProperty("type").GetString() == "enum";
    public bool IsNumber => !IsBoolean && !IsChoice;
    public bool Editable => !definition.TryGetProperty("lockedValue", out _);
    public string LockReason => Editable ? "" : Localized("lockReason") ?? "";
    public bool HasLockReason => LockReason.Length > 0;
    public CultureInfo Culture => _culture;
    public IReadOnlyList<string> Choices => IsChoice ? definition.GetProperty("choices").EnumerateArray().Select(c => c.GetProperty("value").GetString()!).ToArray() : [];
    private string _text = ManifestIni.Display(definition.TryGetProperty("lockedValue", out var locked) ? locked : value, CultureInfo.GetCultureInfo(locale));
    public string Text { get => _text; set { if (_text == value) return; _text = value; PropertyChanged?.Invoke(this, new(nameof(Text))); PropertyChanged?.Invoke(this, new(nameof(Checked))); changed(); } }
    public bool Checked { get => _text is "1" or "true"; set => Text = value ? "1" : "0"; }
    public event PropertyChangedEventHandler? PropertyChanged;
    private string? Localized(string field) => definition.TryGetProperty(field, out var texts)
        ? (texts.TryGetProperty(locale, out var text) ? text : texts.GetProperty("en")).GetString()! : null;
}

public sealed partial class ShellViewModel
{
    private static readonly HttpClient ModDownloads = new(new HttpClientHandler { AllowAutoRedirect = false, UseCookies = false, Credentials = null }) { Timeout = Timeout.InfiniteTimeSpan };
    private bool _onMods, _showModOptions, _iniDirty, _modsStarted;
    private ManifestIni? _ini;
    private string? _iniManifest;
    public ObservableCollection<ManifestOptionItem> ModOptions { get; } = [];
    public bool OnMods => _onMods;
    public bool ShowMods => (InShell || DevelopmentMods) && _onMods && !ShowPickSave;
    public bool CanOpenMods => (InShell && _install is not null || DevelopmentMods) && !ModOperationBusy;
    public bool ShowModOptions => _showModOptions && ShowMods && _ini is not null && _externalMod is null;
    public bool ModsIdle => !_busy && !Playing && !LauncherUpdateBusy && !IntegrityChecking;
    public bool ModsNews => _catalogLoaded && !_onMods && ModCards.Any(c => c.CanAct && (c.UpdateAvailable || c.Action == S["mods.install"]));
    public string ModsTag => S["mods.tag_new"];
    public bool IniDirty => _iniDirty && ModsIdle;
    public void OpenMods()
    {
        if (!CanOpenMods) return;
        LeaveAccount(); _onMods = true; _onSetup = false; _onBugReport = false; _onNews = false;
        Changed(string.Empty);
        _ = _catalogLoaded ? RefreshModsAsync() : LoadModCatalogQuietlyAsync();
    }
    public void OpenHome() { LeaveAccount(); _onMods = false; _onSetup = false; _onBugReport = false; _onNews = false; Changed(string.Empty); }

    /// <summary>When an Azimuth folder is found: state in the background, and once per run the catalog (news tag) and retention.</summary>
    private void StartMods()
    {
        _ = RefreshModsAsync();
        if (_modsStarted) { EnsureModCatalog(); return; } // T1f: a game folder found or changed in the shell loads it too
        _modsStarted = true;
        _ = StartModsOnceAsync();
    }

    private async Task StartModsOnceAsync()
    {
        await MaintainModsAsync();
        if (InShell || DevelopmentMods) await LoadModCatalogQuietlyAsync();
    }

    private void RefreshMods() => _ = RefreshModsAsync();

    private async Task LoadSelectedOptionsAsync()
    {
        _ini = null; _iniManifest = null; _iniDirty = false; ModOptions.Clear();
        if (_selectedMod is not { HasOptions: true } selected || ModManagerFactory() is not { } factory) return;
        try
        {
            var root = _install?.Root;
            // OD-125/127: ExtraOptions.Infinity's own ini, beside azimuth.exe; no managed install behind it
            var ini = await Task.Run(() => selected.BuiltIn && root is not null
                ? Infinity.Core.Process.ExtraOptionsInfinity.ReadSettings(root) : factory().ReadSettings(selected.Manifest));
            if (!ReferenceEquals(_selectedMod, selected) && _selectedMod?.Manifest.Digest != selected.Manifest.Digest) return;
            _ini = ini; _iniManifest = selected.Manifest.Digest; ModOptions.Clear();
            foreach (var option in selected.Manifest.Document.GetProperty("config").GetProperty("options").EnumerateArray())
                ModOptions.Add(new(option, _ini.Values[option.GetProperty("id").GetString()!], S.Locale,
                    () => { _iniDirty = true; Changed(nameof(IniDirty)); }));
        }
        catch (Exception e) when (ManagedModFiles.IsStateError(e))
        { _ini = null; Message = S["mods.settings_changed"]; }
        Changed(nameof(ShowModOptions));
    }

    public void SaveIni()
    {
        if (_ini is null || _selectedMod is not { } selected || !ModsIdle || ModManagerFactory() is not { } factory) return;
        var ini = _ini; var root = _install?.Root;
        var edits = ModOptions.Where(o => o.Editable).Select(o => (o.Id, o.Text, o.Culture)).ToArray();
        _ = RunModOperationAsync(() => Task.Run(() =>
        {
            foreach (var option in edits) ini.Set(option.Id, option.Text, option.Culture);
            if (selected.BuiltIn && root is not null) Infinity.Core.Process.ExtraOptionsInfinity.SaveSettings(root, ini);
            else factory().SaveSettings(selected.Manifest, ini);
        }), S["mods.options_saved"], preserveOptionsOnFailure: true);
    }
    public async Task RevertIniAsync() { await LoadSelectedOptionsAsync(); Changed(string.Empty); }
    public void RevertIni() => _ = RevertIniAsync();
    private static bool GameRunning()
    {
        var processes = System.Diagnostics.Process.GetProcessesByName("azimuth");
        try { return processes.Length > 0; } finally { foreach (var process in processes) process.Dispose(); }
    }

    /// <summary>
    /// Before every launch, off the UI thread (H4d, §1.3): locked settings enforced and Wine overrides
    /// reconciled. Nothing installed: nothing read or written.
    /// </summary>
    private async Task<ModLaunchPreparation> PrepareModsForLaunchAsync(bool hardcore = false)
    {
        if (ModManagerFactory() is not { } factory) return ModLaunchPreparation.None;
        ModLaunchPreparation prepared;
        for (var attempt = 0; ; attempt++)
        {
            // Startup retention may hold the bucket lock for a moment: wait for it rather than refuse the launch.
            try { prepared = await Task.Run(() => factory().PrepareForLaunch(hardcore)); break; }
            catch (ModException error) when (error.Code == ModErrors.Busy && attempt < 20) { await Task.Delay(250); }
        }
        foreach (var warning in prepared.Warnings) Log("mods: " + warning);
        return prepared;
    }
}
