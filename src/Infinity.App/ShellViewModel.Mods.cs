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
    // OD-206 (W11): the page lists the mods that declare the chosen game (none declared: FH2) and that game's built-in options card
    public bool CanOpenMods => (InShell && _install is not null || DevelopmentMods) && !ModOperationBusy && CurrentGame.HasModsPage;
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
        // the read started at launch may still run: the page shows its progress and fills when it ends (owner 2026-11-10). Build #3: opening
        // the page never reads every mod again; only a page that no read has filled yet asks for one (↻ reads on request)
        _ = GuardAsync("open Mods", () => !_catalogLoaded ? LoadModCatalogQuietlyAsync() : EnsureModsReadAsync());
    }
    public void OpenHome() { LeaveAccount(); _onMods = false; _onSetup = false; _onBugReport = false; _onNews = false; _onProfiles = false; FillPlayBoard(); Changed(string.Empty); }

    /// <summary>When an Azimuth folder is found: state in the background, and once per run the catalog (news tag) and retention.</summary>
    private void StartMods()
    {
        // OD-233: the per-game mods folders before the Mods page first reads them. The receipts decide their own folders' games, so those
        // move now even without the catalog (an installed mod is never shown as an unreadable record of the old layout); a folder copied
        // by hand waits for the catalog to name its game (after it loads, or before Play). A stop is shown on Play and Mods, and retried.
        var first = !_modsStarted;
        _modsStarted = true;
        _ = GuardAsync("start the mods", () => StartModsAsync(first));
    }

    private async Task StartModsAsync(bool first)
    {
        // the process list off the UI thread too (owner 2026-11-10: the launcher never looks frozen)
        if (_install is not null && !await Task.Run(GameRunning)) await PrepareModsLayoutAsync(receiptsOnly: !_catalogLoaded, background: true);
        // owner 2026-11-10: FM5 takes hand-copied mods again; what its former catalog-only rule held off is turned back on (once)
        await RestoreFm5CatalogOnlyHoldAsync();
        await RefreshModsAsync();
        if (!first) { EnsureModCatalog(); return; } // T1f: a game folder found or changed in the shell loads it too
        await StartModsOnceAsync();
    }

    private async Task StartModsOnceAsync()
    {
        await MaintainModsAsync();
        if (InShell || DevelopmentMods) await LoadModCatalogQuietlyAsync();
    }

    private void RefreshMods() => _ = GuardAsync("read the mods", RefreshModsAsync);

    private async Task LoadSelectedOptionsAsync()
    {
        _ini = null; _iniManifest = null; _iniDirty = false; ModOptions.Clear();
        if (_selectedMod is not { HasOptions: true } selected || ModManagerFactory() is not { } factory) return;
        try
        {
            var root = _install?.Root;
            // OD-125/127: ExtraOptions.Infinity's own ini, beside azimuth.exe; no managed install behind it
            var game = CurrentGame;
            var ini = await Task.Run(() => selected.BuiltIn && root is not null
                ? Infinity.Core.Process.ExtraOptionsInfinity.ReadSettings(root, game) : factory().ReadSettings(selected.Manifest));
            if (!ReferenceEquals(_selectedMod, selected) && _selectedMod?.Manifest.Digest != selected.Manifest.Digest) return;
            _ini = ini; _iniManifest = selected.Manifest.Digest; ModOptions.Clear();
            foreach (var option in selected.Manifest.Document.GetProperty("config").GetProperty("options").EnumerateArray())
                ModOptions.Add(new(option, _ini.Values[option.GetProperty("id").GetString()!], S.Locale,
                    () => { _iniDirty = true; Changed(nameof(IniDirty)); }));
        }
        catch (Exception e) when (e is System.IO.DirectoryNotFoundException or System.IO.FileNotFoundException)
        {
            // hotfix 3.1.2: the mod's folder (or its settings file) is not on this PC: say so, never "the settings changed"
            Log("mods: options not read (" + e.GetType().Name + ", " + selected.Manifest.Id + ": its folder or settings file is missing)");
            _ini = null; ModOptions.Clear();
            Message = S.Format("mods.options_missing", ("name", selected.Manifest.Text("name", S.Locale)));
        }
        catch (Exception e) when (ManagedModFiles.IsStateError(e))
        { Log("mods: options not read (" + e.GetType().Name + (e is ModException mod ? " " + mod.Code : "") + ")"); _ini = null; Message = S["mods.settings_changed"]; }
        catch (Exception e) when (e is InvalidOperationException or KeyNotFoundException or ArgumentException or FormatException)
        { Log("mods: options not shown: " + ErrorChain(e)); _ini = null; ModOptions.Clear(); Message = S["mods.options_unreadable"]; }
        Changed(nameof(ShowModOptions));
    }

    public void SaveIni()
    {
        // owner 2026-11-10: never a silent no-op; the draft stays and the pane says what Save waits for
        if (!ModsIdle) { Message = ModOptionsWait; Changed(nameof(ModOptionsWait)); return; }
        if (_ini is null || _selectedMod is not { } selected || ModManagerFactory() is not { } factory) return;
        var ini = _ini; var root = _install?.Root; var game = CurrentGame;
        var edits = ModOptions.Where(o => o.Editable).Select(o => (o.Id, o.Text, o.Culture)).ToArray();
        _ = RunModOperationAsync(() => Task.Run(() =>
        {
            foreach (var option in edits) ini.Set(option.Id, option.Text, option.Culture);
            if (selected.BuiltIn && root is not null) Infinity.Core.Process.ExtraOptionsInfinity.SaveSettings(root, ini, game);
            else factory().SaveSettings(selected.Manifest, ini);
        }), S["mods.options_saved"], preserveOptionsOnFailure: true, label: S.Format("mods.lock.settings", ("mod", ModName(selected.Manifest))),
            only: selected.BuiltIn ? [] : [selected.Manifest.Id]); // the built-in options change no managed mod: the cards are rebuilt only
    }
    public async Task RevertIniAsync()
    {
        if (!ModsIdle) { Message = ModOptionsWait; Changed(nameof(ModOptionsWait)); return; }
        await LoadSelectedOptionsAsync(); Changed(string.Empty);
    }
    public void RevertIni() => _ = GuardAsync("undo the options", RevertIniAsync);
    private static bool GameRunning()
    {
        var processes = System.Diagnostics.Process.GetProcessesByName("azimuth");
        try { return processes.Length > 0; } finally { foreach (var process in processes) process.Dispose(); }
    }

    /// <summary>The launcher's mods a Hardcore launch of <paramref name="game"/> turned off, to give back at the next Noob or offline launch.</summary>
    private string ManagedHardcoreHeldFile(string game) => System.IO.Path.Combine(_stateDirectory, "managed-hardcore-held-" + game + ".json");

    /// <summary>
    /// Before every launch, off the UI thread (H4d, §1.3): locked settings enforced and Wine overrides
    /// reconciled. Nothing installed: nothing read or written. Hardcore turns off what it does not keep first (HoldForHardcore).
    /// </summary>
    private async Task<ModLaunchPreparation> PrepareModsForLaunchAsync(bool hardcore = false)
    {
        // OD-233: FM5 has its own mods (locks, Wine overrides, Hardcore's refusal), each launch only its own game's
        if (!Infinity.Core.Mods.ExtraOptionsLayout.HasModsFolder(CurrentGame.Id) || ModManagerFactory() is not { } factory) return ModLaunchPreparation.None;
        var game = CurrentGame.Id;
        var heldFile = ManagedHardcoreHeldFile(game);
        ModLaunchPreparation prepared; var hold = ModHoldResult.None;
        for (var attempt = 0; ; attempt++)
        {
            // Startup retention may hold the bucket lock for a moment: wait for it rather than refuse the launch.
            // Alex 2026-11-10: Hardcore turns off (and the next Noob launch gives back) what it does not keep, never a refusal
            try
            {
                (hold, prepared) = await Task.Run(() =>
                {
                    var manager = factory();
                    var held = manager.HoldForHardcore(game, hardcore, heldFile);
                    return (held, manager.PrepareForLaunch(hardcore, game));
                });
                break;
            }
            catch (ModException error) when (error.Code == ModErrors.Busy && attempt < 20) { await Task.Delay(250); }
        }
        foreach (var note in hold.Notes) Log(note);
        foreach (var warning in prepared.Warnings) Log("mods: " + warning);
        // only the mods it switched are read again (owner 2026-11-10: no full read at PLAY)
        if (hold.Changed.Count > 0) await RefreshModEntriesAsync(hold.Changed);
        return prepared;
    }
}
