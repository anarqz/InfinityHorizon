// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Mods;

namespace Infinity.App;

public sealed class ManifestOptionItem(JsonElement definition, JsonElement value, string locale, Action changed) : INotifyPropertyChanged
{
    public string Id => definition.GetProperty("id").GetString()!;
    public string Label => Localized("label");
    public string Help => Localized("description");
    public bool IsBoolean => definition.GetProperty("type").GetString() == "boolean";
    public bool IsChoice => definition.GetProperty("type").GetString() == "enum";
    public bool IsNumber => !IsBoolean && !IsChoice;
    public bool Editable => !definition.TryGetProperty("lockedValue", out _);
    public IReadOnlyList<string> Choices => IsChoice ? definition.GetProperty("choices").EnumerateArray().Select(c => c.GetProperty("value").GetString()!).ToArray() : [];
    private string _text = ManifestIni.Encode(definition.TryGetProperty("lockedValue", out var locked) ? locked : value);
    public string Text { get => _text; set { if (_text == value) return; _text = value; PropertyChanged?.Invoke(this, new(nameof(Text))); PropertyChanged?.Invoke(this, new(nameof(Checked))); changed(); } }
    public bool Checked { get => _text is "1" or "true"; set => Text = value ? "1" : "0"; }
    public event PropertyChangedEventHandler? PropertyChanged;
    private string Localized(string field) => definition.TryGetProperty(field, out var texts)
        ? (texts.TryGetProperty(locale, out var text) ? text : texts.GetProperty("en")).GetString()! : Id;
}

public sealed partial class ShellViewModel
{
    private static readonly HttpClient ModDownloads = new(new HttpClientHandler { AllowAutoRedirect = false, UseCookies = false, Credentials = null }) { Timeout = Timeout.InfiniteTimeSpan };
    private bool _onMods, _showModOptions, _iniDirty;
    private ManifestIni? _ini;
    private string? _iniManifest;
    public ObservableCollection<ManifestOptionItem> ModOptions { get; } = [];
    public bool OnMods => _onMods;
    public bool ShowMods => (SignedIn || DevelopmentMods) && _onMods && !ShowPickSave;
    public bool CanOpenMods => (SignedIn && _install is not null || DevelopmentMods) && !ModOperationBusy;
    public bool ShowModOptions => _showModOptions && ShowMods && _ini is not null;
    public bool ModsIdle => !_busy && !Playing && !LauncherUpdateBusy && !IntegrityChecking;
    public bool ModsNews => _catalogLoaded && !_onMods && ModCards.Any(c => c.CanAct && c.Action == S["mods.install"]);
    public string ModsTag => S["mods.tag_new"];
    public bool IniDirty => _iniDirty && ModsIdle;
    public void OpenMods()
    {
        if (!CanOpenMods) return;
        LeaveAccount(); _onMods = true; _onSetup = false; _onBugReport = false;
        RefreshMods(); Changed(string.Empty);
        if (DevelopmentMods && !_catalogLoaded) _ = LoadModCatalogAsync();
    }
    public void OpenHome() { LeaveAccount(); _onMods = false; _onSetup = false; _onBugReport = false; Changed(string.Empty); }
    private void StartMods() { RefreshMods(); }
    private void RefreshMods() { RefreshModCards(); if (_selectedMod is { HasOptions: true }) LoadSelectedOptions(); }
    private void LoadSelectedOptions()
    {
        _ini = null; _iniManifest = null; _iniDirty = false; ModOptions.Clear();
        if (_install is null || _selectedMod is not { HasOptions: true } selected) return;
        try
        {
            _ini = ManagedMods().ReadSettings(selected.Manifest);
            _iniManifest = selected.Manifest.Digest;
            foreach (var option in selected.Manifest.Document.GetProperty("config").GetProperty("options").EnumerateArray())
                ModOptions.Add(new(option, _ini.Values[option.GetProperty("id").GetString()!], S.Locale,
                    () => { _iniDirty = true; Changed(nameof(IniDirty)); }));
        }
        catch (Exception e) when (e is ModException or IOException or UnauthorizedAccessException or JsonException)
        { _ini = null; Message = S["mods.settings_changed"]; }
    }
    public void SaveIni()
    {
        if (_ini is null || _selectedMod is not { } selected || !ModsIdle) return;
        var ini = _ini;
        var edits = ModOptions.Select(o => (o.Id, o.Text)).ToArray();
        _ = RunModOperationAsync(() => Task.Run(() =>
        {
            foreach (var option in edits) ini.Set(option.Id, option.Text);
            ManagedMods().SaveSettings(selected.Manifest, ini);
        }), S["mods.options_saved"], preserveOptionsOnFailure: true);
    }
    public void RevertIni() { LoadSelectedOptions(); Changed(string.Empty); }
    private static bool GameRunning()
    {
        var processes = System.Diagnostics.Process.GetProcessesByName("azimuth");
        try { return processes.Length > 0; } finally { foreach (var process in processes) process.Dispose(); }
    }
    private void ValidateManagedMods()
    {
        if (_install is null) return;
        var manager = ManagedMods();
        foreach (var manifest in manager.InstalledManifests().Where(m => manager.Receipt(m) is { Enabled: true }))
        {
            if (!manager.Verified(manifest)) throw new ModException("managed mod files changed");
            manager.CheckDependencies(manifest);
            if (manifest.ConfigPath is not null)
            {
                var settings = manager.ReadSettings(manifest); var before = settings.ToBytes(); settings.EnforceLocks();
                if (!before.SequenceEqual(settings.ToBytes())) manager.SaveSettings(manifest, settings);
            }
        }
    }
}
