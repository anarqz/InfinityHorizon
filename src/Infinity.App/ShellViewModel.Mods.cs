// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.ComponentModel;
using System.IO;
using System.Net.Http;
using System.Text.Json;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>One switch of the ExtraOptions form.</summary>
public sealed class IniOptionItem(IniOption option, string label, string help, bool value, Action changed) : INotifyPropertyChanged
{
    /// <summary>False for a switch Infinity keeps off (it conflicts with Infinity online).</summary>
    public bool Editable => !Option.LockedOff;

    private bool _value = value;

    public event PropertyChangedEventHandler? PropertyChanged;

    public IniOption Option { get; } = option;

    public string Label { get; } = label;

    public string Help { get; } = help;

    public bool Value
    {
        get => _value;
        set
        {
            if (_value != value)
            {
                _value = value;
                PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(nameof(Value)));
                changed();
            }
        }
    }
}

/// <summary>One INI section of the form.</summary>
public sealed record IniGroup(string Title, string? Warning, IReadOnlyList<IniOptionItem> Options)
{
    public bool HasWarning => Warning is not null;
}

/// <summary>
/// OD-45: the Mods screen (docs/launcher/MODS.md). The catalog's mods only; on/off, install and
/// update to the vetted release built for this Azimuth (SHA-256 + MD5), the INI as a form in a
/// sidebar, and the author's credit and link. Updates are checked when the install is found; the
/// menu's tag shows news (a newer release on GitHub, or a mod the player never opened here).
/// Nothing changes while the game runs, a session is starting, or another action is running.
/// </summary>
public sealed partial class ShellViewModel
{
    private static readonly HttpClient ModDownloads = new(new HttpClientHandler
    {
        AllowAutoRedirect = false, UseCookies = false, Credentials = null,
    }) { Timeout = Timeout.InfiniteTimeSpan };
    private static readonly JsonSerializerOptions SeenJson = new() { WriteIndented = true };
    private bool _onMods;
    private bool _showModOptions;
    private ModState? _eo;
    private ModUpdate? _eoUpdate;
    private bool _eoChecked;
    private bool _eoChecking;
    private string? _eoCheckError;
    private ExtraOptionsIni? _ini;
    private bool _iniDirty;

    public ObservableCollection<IniGroup> IniGroups { get; } = [];

    public bool OnMods => _onMods;

    public bool ShowMods => (SignedIn || DevelopmentMods) && _onMods && !ShowPickSave;

    public bool CanOpenMods => (SignedIn && _install is not null || DevelopmentMods) && !ModOperationBusy;

    /// <summary>The options sidebar is open on the Mods screen.</summary>
    public bool ShowModOptions => _showModOptions && ShowMods && _ini is not null;

    [System.Diagnostics.CodeAnalysis.SuppressMessage("Performance", "CA1822:Mark members as static", Justification = "XAML binds to instance properties")]
    public ModInfo Eo => ModCatalog.ExtraOptions;

    /// <summary>
    /// The menu's tag: a mod the player never opened here, or (after this run's check) a newer
    /// release on GitHub than the one last seen. A failed or pending check never raises it.
    /// </summary>
    public bool ModsNews => _install is not null && (Seen(Eo.Id) is not { } seen || (_eoChecked && _eoUpdate is { } u && u.Version.ToString() != seen));

    public string ModsTag => S["mods.tag_new"];

    public string EoCredits => S.Format("mods.credits", ("author", Eo.Author), ("license", Eo.License));

    public string EoRepository => Eo.Repository;

    public bool EoInstalled => _eo?.Installed == true;

    /// <summary>The vetted release built for this Azimuth (null: none, or the version is unknown).</summary>
    public ModRelease? EoTarget => Eo.For(_install?.Version);

    /// <summary>A change is safe now: no game, no session starting, nothing else running.</summary>
    public bool ModsIdle => !_busy && !Playing && !LauncherUpdateBusy && !IntegrityChecking;

    public bool CanToggleEo => EoInstalled && ModsIdle && !HasManagedModState;

    public bool EoEnabled
    {
        get => _eo?.Enabled == true;
        set
        {
            if (!CanToggleEo || _eo is null || !_eo.Installed || value == _eo.Enabled)
            {
                return;
            }
            ModAction(() => Mods().SetEnabled(Eo, value), value ? S["mods.turned_on"] : S["mods.turned_off"]);
        }
    }

    public string EoStatus => _eo switch
    {
        null or { Installed: false } => S["mods.not_installed"],
        { Official: true } s => S.Format(s.Enabled ? "mods.on_version" : "mods.off_version", ("version", s.Version!.Version)),
        { Enabled: true } => S["mods.on_unknown"],
        _ => S["mods.off_unknown"],
    };

    /// <summary>The integrity line: official build (MD5) or not.</summary>
    public string EoIntegrity => _eo switch
    {
        null or { Installed: false } => string.Empty,
        { Official: true } s => S.Format("mods.md5_official", ("version", s.Version!.Version), ("md5", s.BinaryMd5!)),
        { } s => S.Format("mods.md5_unknown", ("md5", s.BinaryMd5 ?? "?")),
    };

    public bool EoOfficial => _eo?.Official == true;

    public bool EoWarning => _eo is { Installed: true, Official: false };

    /// <summary>The installed official build is for another Azimuth, or there is none for this one.</summary>
    public bool EoAzimuthMismatch => _install is not null && (EoTarget is null || (_eo?.Version is { } r && r.AzimuthVersion != _install.Version));

    public string EoAzimuthText => EoTarget is null
        ? S.Format("mods.no_build_for_azimuth", ("version", _install?.Version ?? "?"))
        : S.Format("mods.azimuth_mismatch", ("version", _eo?.Version?.AzimuthVersion ?? "?"));

    public string EoUpdateText
    {
        get
        {
            if (_eoChecking)
            {
                return S["mods.checking"];
            }
            var lines = new List<string>();
            if (EoTarget is { } target && _eo is not null && NeedsInstall(_eo, target))
            {
                lines.Add(S.Format(_eo.Installed ? "mods.update_ready" : "mods.install_ready", ("version", target.Version)));
            }
            else if (EoTarget is not null && _eo is { Official: true })
            {
                lines.Add(S["mods.up_to_date"]);
            }
            if (_eoUpdate is { Verified: false } news && (EoTarget is null || news.Version > ModVersion.Parse(EoTarget.Version)))
            {
                lines.Add(S.Format("mods.update_unvetted", ("version", news.Version.ToString())));
            }
            if (_eoCheckError is not null)
            {
                lines.Add(S["mods.check_failed"]);
            }
            return string.Join(" ", lines);
        }
    }

    /// <summary>Install, update, or repair (an unrecognized binary) to the vetted release for this Azimuth.</summary>
    public bool CanInstallEo => ModsIdle && !HasManagedModState && EoTarget is { } target && _eo is not null && NeedsInstall(_eo, target);

    public string EoInstallLabel => _eo is { Installed: true, Official: true } ? S["mods.update"] : _eo is { Installed: true } ? S["mods.repair"] : S["mods.install"];

    public bool HasIni => _ini is not null;

    public bool IniDirty => _iniDirty;

    private static bool NeedsInstall(ModState state, ModRelease target) =>
        !state.Installed || !state.Official || ModVersion.Parse(target.Version) > ModVersion.Parse(state.Version!.Version);

    public void OpenMods()
    {
        if (!CanOpenMods)
        {
            return;
        }
        LeaveAccount();
        _onMods = true;
        _onSetup = false;
        _onBugReport = false;
        RefreshMods();
        MarkNewsSeen();
        Changed(string.Empty);
        if (DevelopmentMods && !_catalogLoaded) _ = LoadModCatalogAsync();
        if (!_eoChecked && !_eoChecking)
        {
            _ = CheckModsAsync();
        }
    }

    public void OpenHome()
    {
        LeaveAccount();
        _onMods = false;
        _onSetup = false;
        _onBugReport = false;
        Changed(string.Empty);
    }

    public void ToggleModOptions()
    {
        _showModOptions = !_showModOptions;
        if (_showModOptions)
        {
            RefreshMods();
        }
        Changed(string.Empty);
    }

    /// <summary>When the launcher finds the install (startup): read the folder, then ask GitHub for news.</summary>
    private void StartMods()
    {
        RefreshMods();
        if (!DevelopmentMods) _ = CheckModsAsync();
    }

    public async Task CheckModsAsync()
    {
        if (_eoChecking)
        {
            return;
        }
        _eoChecking = true;
        _eoCheckError = null;
        Changed(string.Empty);
        try
        {
            using var bound = new CancellationTokenSource(TimeSpan.FromSeconds(20));
            _eoUpdate = await ModManager.CheckAsync(Http, Eo, bound.Token);
            _eoChecked = true;
            Log(_eoUpdate is null ? "mods: no ExtraOptions release found" : $"mods: ExtraOptions latest {_eoUpdate.Version} (vetted: {_eoUpdate.Verified})");
        }
        catch (Exception e) when (e is HttpRequestException or TaskCanceledException or JsonException or InvalidOperationException
                                    or KeyNotFoundException or FormatException or ArgumentException or OverflowException)
        {
            _eoCheckError = e.Message;
            Log("mods: update check failed: " + e.Message);
        }
        finally
        {
            _eoChecking = false;
            if (_onMods)
            {
                MarkNewsSeen();
            }
            Changed(string.Empty);
        }
    }

    public async Task InstallEoAsync()
    {
        if (!CanInstallEo || EoTarget is not { } release)
        {
            return;
        }
        if (GameRunning())
        {
            Message = S["mods.close_game"];
            return;
        }
        // Always start at the catalog's official release URL, then verify the downloaded bytes.
        var url = release.DownloadUrl(Eo);
        _modOperation = true; _modProgress = S["mods.progress.download"];
        Busy = true;
        Message = S.Format("mods.installing", ("version", release.Version));
        try
        {
            using var bound = new CancellationTokenSource(TimeSpan.FromMinutes(3));
            var backup = await Mods().InstallAsync(ModDownloads, Eo, release, url, bound.Token);
            Log($"mods: ExtraOptions {release.Version} installed (backup {backup})");
            Message = S.Format("mods.installed", ("version", release.Version));
        }
        catch (Exception e) when (e is ModException or HttpRequestException or TaskCanceledException or IOException
                                    or UnauthorizedAccessException or InvalidDataException)
        {
            Log("mods: install failed: " + e.GetType().Name);
            Message = S[e switch
            {
                HttpRequestException or TaskCanceledException => "mods.download_failed",
                IOException io when (io.HResult & 0xffff) is 225 or 226 => "mods.antivirus_blocked",
                UnauthorizedAccessException or IOException => "mods.write_failed",
                _ => "mods.install_failed",
            }];
        }
        finally
        {
            _modOperation = false;
            Busy = false;
            RefreshMods();
            Changed(string.Empty);
        }
    }

    public void SaveIni()
    {
        if (_ini is null)
        {
            return;
        }
        if (GameRunning() || !ModsIdle || ModRecoveryRequired)
        {
            // the player's switches stay on screen; nothing is written
            Message = S["mods.close_game"];
            return;
        }
        foreach (var item in IniGroups.SelectMany(g => g.Options))
        {
            if (_ini.Effective(item.Option) != item.Value)
            {
                _ini.Set(item.Option, item.Value);
            }
        }
        _ini.EnforceInfinityLocks();
        ModAction(() => Mods().SaveIni(Eo, _ini, DateTime.UtcNow), S["mods.options_saved"]);
    }

    public void RevertIni()
    {
        RefreshMods();
        Message = null;
        Changed(string.Empty);
    }

    private void ModAction(Action action, string done) =>
        _ = RunModOperationAsync(() => Task.Run(action), done);

    private ModManager Mods() => new(_install!.Root, GameRunning);

    private static bool GameRunning() => System.Diagnostics.Process.GetProcessesByName("azimuth").Length > 0;

    /// <summary>
    /// Before an Infinity launch: the ExtraOptions switches that conflict with Infinity are put back
    /// off (a backup first). Never blocks the launch.
    /// </summary>
    private void EnforceModLocks()
    {
        if (_install is null || GameRunning())
        {
            return;
        }
        try
        {
            if (Mods().EnforceInfinityLocks(Eo, DateTime.UtcNow))
            {
                Log("mods: ExtraOptions switches that conflict with Infinity were turned off");
            }
        }
        catch (Exception e) when (e is ModException or IOException or UnauthorizedAccessException)
        {
            Log("mods: " + e.Message);
        }
    }

    /// <summary>Re-reads the folder and the INI into the form.</summary>
    private void RefreshMods()
    {
        if (_install is null)
        {
            return;
        }
        try
        {
            _eo = Mods().State(Eo);
            var path = Path.Combine(_install.Root, ExtraOptionsIni.FileName);
            _ini = File.Exists(path) ? ExtraOptionsIni.Load(path) : null;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            Log("mods: " + e.Message);
            _ini = null;
        }
        _iniDirty = false;
        IniGroups.Clear();
        RefreshModCards();
        if (_ini is { } ini)
        {
            var manifest = _manifestCatalog.FirstOrDefault(m => m.Id == Eo.Id && m.ApprovedForFileInstall);
            var options = manifest is null ? ExtraOptionsIni.Schema : manifest.Document.GetProperty("config").GetProperty("options")
                .EnumerateArray().Select(o => new IniOption(o.GetProperty("section").GetString()!, o.GetProperty("key").GetString()!,
                    o.GetProperty("default").GetBoolean(), o.GetProperty("advanced").GetBoolean(),
                    ExtraOptionsIni.Schema.Any(policy => policy.LockedOff && policy.Section == o.GetProperty("section").GetString()
                        && policy.Key == o.GetProperty("key").GetString()))).ToArray();
            foreach (var section in options.GroupBy(o => o.Section))
            {
                var advanced = section.Any(o => o.Advanced);
                IniGroups.Add(new IniGroup(
                    S[$"eo.section.{section.Key.ToLowerInvariant()}"],
                    advanced ? S["eo.advanced_warning"] : null,
                    [.. section.Select(o => new IniOptionItem(o, S[o.StringKey],
                        o.LockedOff ? S[o.StringKey + ".help"] + " " + S["eo.locked"] : S[o.StringKey + ".help"],
                        !o.LockedOff && ini.Effective(o), IniChanged))]));
            }
        }
    }

    private void IniChanged()
    {
        _iniDirty = true;
        Changed(nameof(IniDirty));
    }

    /// <summary>The screen was seen: the checked newest release, or (no check yet) the catalog's.</summary>
    private void MarkNewsSeen()
    {
        if (_eoChecked && _eoUpdate is { } u)
        {
            MarkSeen(Eo.Id, u.Version.ToString());
        }
        else if (Seen(Eo.Id) is null)
        {
            MarkSeen(Eo.Id, Eo.Latest.Version);
        }
    }

    private string SeenPath => Path.Combine(_stateDirectory, "mods-seen.json");

    private string? Seen(string id)
    {
        try
        {
            return JsonSerializer.Deserialize<Dictionary<string, string>>(File.ReadAllText(SeenPath))?.GetValueOrDefault(id);
        }
        catch (Exception e) when (e is IOException or JsonException or UnauthorizedAccessException)
        {
            return null;
        }
    }

    private void MarkSeen(string id, string version)
    {
        try
        {
            Dictionary<string, string> seen;
            try
            {
                seen = JsonSerializer.Deserialize<Dictionary<string, string>>(File.ReadAllText(SeenPath)) ?? [];
            }
            catch (Exception e) when (e is IOException or JsonException)
            {
                seen = [];
            }
            seen[id] = version;
            Directory.CreateDirectory(_stateDirectory);
            File.WriteAllText(SeenPath, JsonSerializer.Serialize(seen, SeenJson));
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            Log("mods: " + e.Message);
        }
    }
}
