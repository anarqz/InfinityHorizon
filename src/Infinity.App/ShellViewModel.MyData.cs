// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.ObjectModel;
using System.Globalization;
using System.IO;
using System.Text.Json;
using Infinity.Core.PlayerData;
using Infinity.Core.Saves;

namespace Infinity.App;

public sealed record DataSaveRow(DataSave Save, string Title, string Stats, string State)
{
    public string Path => Save.RelativePath;
}
public sealed record DataTarget(string Folder, string Label);
public sealed record DataCacheRow(string Title, string Summary, string Location);

public sealed partial class ShellViewModel
{
    private bool _onMyData;
    private DataSaveRow? _dataSelected;
    private DataTarget? _dataTarget;
    private CancellationTokenSource? _dataScan;
    private PendingDataAction? _dataAction;
    private string _dataStatus = string.Empty;
    private string _dataScanNote = string.Empty;
    private string _dataOtherCaches = string.Empty;
    private bool _dataCacheRecovery, _dataLoaded;
    private sealed record PendingDataAction(string Root, DataSave? Source, string? Target, string Kind, string[] LocalProfiles);

    public bool ShowMyData => SignedIn && _onMyData && !ShowPickSave;
    public bool DataIdle => SignedIn && !Busy && !Playing && _install is not null;
    public bool DataCanRefresh => DataIdle;
    public bool DataScanning => _dataScan is not null;
    public bool DataNoInstall => _install is null;
    public bool DataEmpty => _dataLoaded && DataSaves.Count == 0;
    public bool DataCanBackup => DataIdle && _dataSelected?.Save.Manageable == true && DataMaintenanceReady;
    public bool DataCanInstall => DataCanBackup && _dataSelected?.Save.Sealed == true && _dataTarget is not null &&
        (_dataSelected.Save.Location != DataSaveLocation.Active || !string.Equals(_dataSelected.Save.Folder, _dataTarget.Folder, StringComparison.OrdinalIgnoreCase));
    public bool DataCanSwap => DataCanInstall && _dataSelected?.Save.Location == DataSaveLocation.Active &&
        DataTargets.Any(target => string.Equals(target.Folder, _dataSelected.Save.Folder, StringComparison.OrdinalIgnoreCase));
    public bool DataCanErase => DataCanBackup && (_dataSelected?.Save.Location != DataSaveLocation.Active ||
        DataTargets.Any(target => string.Equals(target.Folder, _dataSelected.Save.Folder, StringComparison.OrdinalIgnoreCase)));
    public bool DataCanMove => DataCanInstall && DataCanErase;
    public bool DataHasConfirmation => _dataAction is not null;
    public bool DataCanConfirm => _dataAction is not null && DataIdle;
    public bool DataCanClearCache => DataIdle && _dataLoaded && !_dataCacheRecovery && !DataHasConfirmation;
    public bool DataPendingRecovery => !DataMaintenanceReady;
    public string DataStatus => _dataStatus;
    public string DataScanNote => _dataScanNote;
    public string DataOtherCaches => _dataOtherCaches;
    public string DataRoot => _install?.Root ?? S["data.no_install"];
    public string DataSaveCount => S.Format("data.save_count", ("count", DataSaves.Count.ToString(CultureInfo.InvariantCulture)));
    public ObservableCollection<DataSaveRow> DataSaves { get; } = [];
    public ObservableCollection<DataTarget> DataTargets { get; } = [];
    public ObservableCollection<DataCacheRow> DataCaches { get; } = [];
    public DataSaveRow? DataSelected { get => _dataSelected; set { _dataSelected = value; CancelDataAction(); Changed(string.Empty); } }
    public DataTarget? DataTarget { get => _dataTarget; set { _dataTarget = value; CancelDataAction(); Changed(string.Empty); } }

    public bool DataMaintenanceReady
    {
        get
        {
            try { return _install is null || !SaveMaintenance.HasPendingSwap(_install.Root); }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException) { return false; }
        }
    }

    public string DataConfirmation => _dataAction is not { } action ? string.Empty :
        S.Format("data.confirm_" + action.Kind,
            ("source", action.Source?.RelativePath ?? string.Empty), ("target", action.Target ?? string.Empty));

    public async Task OpenMyDataAsync()
    {
        if (!SignedIn || Busy) return;
        LeaveAccount();
        _onHelp = _onMods = _onSetup = _onBugReport = _crashOpen = false;
        _onMyData = true;
        Changed(string.Empty);
        await RefreshMyDataAsync();
    }

    public async Task RefreshMyDataAsync()
    {
        if (!DataIdle || !_onMyData || _install is not { } install) return;
        var scan = new CancellationTokenSource();
        _dataScan = scan;
        Busy = true;
        CancelDataAction();
        _dataLoaded = false;
        DataSaves.Clear(); DataCaches.Clear(); DataTargets.Clear();
        _dataSelected = null; _dataTarget = null;
        _dataStatus = S["data.scanning"];
        try
        {
            var result = await Task.Run(() => (Saves: new SaveDiscovery(install.Root).Scan(scan.Token), Caches: new CacheDiscovery(install.Root).Scan(scan.Token)), scan.Token);
            if (!ReferenceEquals(_install, install) || !ShowMyData) return;
            foreach (var save in result.Saves.Saves) DataSaves.Add(DescribeDataSave(save));
            var folders = Profiles.Where(p => p.Mode == "noob" && ulong.TryParse(p.Xuid, out _)).Select(p => AzimuthSaves.FolderFor(ulong.Parse(p.Xuid, CultureInfo.InvariantCulture)))
                .Append(AzimuthSaves.FolderFor(AzimuthSaves.OfflineXuid)).Distinct(StringComparer.OrdinalIgnoreCase);
            foreach (var folder in folders)
            {
                var profile = Profiles.FirstOrDefault(p => ulong.TryParse(p.Xuid, out var id) && AzimuthSaves.FolderFor(id).Equals(folder, StringComparison.OrdinalIgnoreCase));
                DataTargets.Add(new(folder, folder == AzimuthSaves.FolderFor(AzimuthSaves.OfflineXuid) ? S["saves.offline_profile"] : profile?.Gamertag ?? folder));
            }
            _dataTarget = DataTargets.FirstOrDefault();
            foreach (var cache in result.Caches.Caches)
            {
                var title = S[cache.Career ? "data.career_cache" : "data.download_cache"] + " · " + S["data.scope_" + cache.Scope.ToString().ToLowerInvariant()];
                var summary = S["data.format_" + cache.Format.ToString().ToLowerInvariant()];
                if (cache.Entries is { } count) summary += " · " + S.Format("data.entries", ("count", count.ToString(CultureInfo.InvariantCulture)));
                if (cache.LastWriteUtc is { } time) summary += "\n" + Bytes(cache.Bytes) + " · " + time.ToLocalTime().ToString("g", CultureInfo.GetCultureInfo(S.Locale));
                DataCaches.Add(new(title, summary, cache.RelativePath));
            }
            _dataScanNote = result.Saves.Limited || result.Saves.Skipped > 0 || result.Caches.Partial ? S["data.partial"] : S["data.scan_complete"];
            _dataOtherCaches = S.Format("data.other_cache", ("count", result.Caches.OtherFiles.ToString(CultureInfo.InvariantCulture)), ("size", Bytes(result.Caches.OtherBytes)));
            _dataCacheRecovery = result.Caches.RecoveryNeeded;
            _dataLoaded = true;
            _dataStatus = _dataCacheRecovery ? S["data.cache_recovery"] : S["data.ready"];
        }
        catch (OperationCanceledException) { _dataStatus = S["data.cancelled"]; }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or JsonException)
        { _dataStatus = S["data.failed"]; }
        finally
        {
            _dataScan = null; scan.Dispose(); Busy = false; Changed(string.Empty);
        }
    }

    private DataSaveRow DescribeDataSave(DataSave save)
    {
        var stats = new List<string>();
        if (save.Level is { } level) stats.Add(S.Format("saves.level", ("level", level.ToString(CultureInfo.GetCultureInfo(S.Locale)))));
        if (save.Credits is { } credits) stats.Add(S.Format("saves.credits", ("credits", credits.ToString("N0", CultureInfo.GetCultureInfo(S.Locale)))));
        if (stats.Count == 0) stats.Add(S["data.stats_unknown"]);
        stats.Add(S.Format("saves.last_played", ("when", save.LastWriteUtc.ToLocalTime().ToString("g", CultureInfo.GetCultureInfo(S.Locale)))));
        stats.Add(Bytes(save.Bytes));
        var profile = Profiles.FirstOrDefault(p => ulong.TryParse(p.Xuid, out var id) && AzimuthSaves.FolderFor(id).Equals(save.Folder, StringComparison.OrdinalIgnoreCase));
        var title = (save.Folder == AzimuthSaves.FolderFor(AzimuthSaves.OfflineXuid) ? S["saves.offline_profile"] : profile?.Gamertag ?? save.Folder) +
            " · " + S.Format("data.revision", ("number", save.Number.ToString("x16", CultureInfo.InvariantCulture)));
        var state = S["data.location_" + save.Location.ToString().ToLowerInvariant()] + " · " +
            S[!save.Manageable ? "data.unsupported" : save.LooseData ? "data.loose" : save.Sealed ? "data.verified" : "data.unverified"];
        return new(save, title, string.Join(" · ", stats), state);
    }

    private static string Bytes(long bytes) => bytes < 1024 ? bytes.ToString("N0", CultureInfo.InvariantCulture) + " B" :
        bytes < 1024 * 1024 ? (bytes / 1024d).ToString("N1", CultureInfo.InvariantCulture) + " KB" :
        (bytes / 1024d / 1024d).ToString("N2", CultureInfo.InvariantCulture) + " MB";
    public void CancelDataScan() => _dataScan?.Cancel();
    private void ResetMyData()
    {
        _dataScan?.Cancel();
        CancelDataAction();
        DataSaves.Clear(); DataCaches.Clear(); DataTargets.Clear();
        _dataSelected = null; _dataTarget = null; _dataLoaded = false;
        _dataStatus = _dataScanNote = _dataOtherCaches = string.Empty;
    }
    public void CancelDataAction() { _dataAction = null; Changed(nameof(DataHasConfirmation)); Changed(nameof(DataCanConfirm)); }

    public void PrepareDataAction(string kind)
    {
        if (!DataIdle || _install is null) return;
        var allowed = kind switch
        {
            "clear" => DataCanClearCache,
            "recover" => DataPendingRecovery,
            "backup" => DataCanBackup,
            "erase" => DataCanErase,
            "copy" => DataCanInstall,
            "move" => DataCanMove,
            "swap" => DataCanSwap,
            _ => false,
        };
        if (!allowed) return;
        _dataAction = new(_install.Root, _dataSelected?.Save, _dataTarget?.Folder, kind, DataTargets.Select(target => target.Folder).ToArray());
        Changed(string.Empty);
    }

    public async Task ConfirmDataActionAsync()
    {
        if (!DataCanConfirm || _dataAction is not { } action || _install?.Root != action.Root || GameRunning())
        { _dataStatus = S["data.close_game"]; CancelDataAction(); Changed(string.Empty); return; }
        CancelDataAction();
        Busy = true;
        var success = false;
        try
        {
            await Task.Run(() =>
            {
                if (action.Kind == "clear") { new CacheDiscovery(action.Root).Clear(); return; }
                var maintenance = new SaveMaintenance(action.Root, action.LocalProfiles);
                if (action.Kind == "recover") { maintenance.RecoverSwaps(); return; }
                if (action.Source is null) throw new IOException("No save selected.");
                var operation = Enum.Parse<SaveDataAction>(action.Kind, true);
                maintenance.Apply(operation, action.Source, action.Target);
            });
            success = true;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or JsonException or SaveException)
        { _dataStatus = S["data.action_failed"]; }
        finally { Busy = false; }
        if (success)
        {
            await RefreshMyDataAsync();
            _dataStatus = S["data.done_" + action.Kind];
        }
        Changed(string.Empty);
    }
}
