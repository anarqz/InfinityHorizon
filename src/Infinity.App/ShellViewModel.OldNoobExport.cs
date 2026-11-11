// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using System.Net.Http;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Saves;
using Infinity.Core.Shell;

namespace Infinity.App;

/// <summary>
/// OD-239 items 6-7: "Export my old Noob save". An AZIMUTH profile has no cloud save, but while the account still has an old Noob cloud line
/// (read-only until its erase), the save home shows a banner and this screen: the line's newest revision is downloaded (SAVES_API §4.2) and
/// written to a new file in a folder the player picks in the launcher's own picker, never into Azimuth's folder. The screen also shows the
/// old local <c>User_&lt;XUID&gt;</c> folder of the profile (OD-120), which the launcher leaves untouched, so progress can be copied by hand.
/// Which lines exist is read from the cached <c>/me</c> only (<see cref="OldNoobExport.Lines"/>, no request).
/// </summary>
public sealed partial class ShellViewModel
{
    private string? _exportLastFolder;
    private string? _exportedFile;

    /// <summary>The account's old Noob lines, from the last <c>/me</c>.</summary>
    private IReadOnlyList<OldNoobLine> OldNoobLines => OfflineMode ? [] : OldNoobExport.Lines(_session?.Me);

    /// <summary>The line the screen exports: the current game's, else the first one.</summary>
    private OldNoobLine? ExportLine => OldNoobLines is [var first, ..] lines ? lines.FirstOrDefault(l => l.Game == CurrentGameId) ?? first : null;

    /// <summary>The save home's banner while an old line exists.</summary>
    public bool ShowOldNoobBanner => ShowSavesHome && ExportLine is not null;

    public bool ShowSavesExport => ShowSaveManager && SavesCurrent == SavesScreen.Export;

    public string OldNoobBannerText => ExportLine is { } line
        ? S.Format("export.banner", ("name", ProfileName.Plain(line.Gamertag)), ("game", GameName(line.Game))) : string.Empty;

    public string ExportLineText => ExportLine is { } line
        ? S.Format("export.line", ("name", ProfileName.Plain(line.Gamertag)), ("game", GameName(line.Game)),
            ("rev", line.Revision.ToString(CultureInfo.CurrentCulture)), ("when", When(line.SavedAt)))
        : S["export.none"];

    /// <summary>The old local folder of the profile in the line's game (<c>…\saves\player-1\User_&lt;XUID&gt;</c>), null without an install.</summary>
    private string? OldNoobFolder => ExportLine is { } line && _install is { } install && GameOf(line.Game) is { } game
        && ulong.TryParse(line.Xuid, NumberStyles.None, CultureInfo.InvariantCulture, out var xuid)
        ? CloudSaveFiles.ProfileDir(AzimuthSaves.SavesRoot(install.Root, game), xuid) : null;

    public bool OldNoobFolderExists => OldNoobFolder is { } folder && Directory.Exists(folder);

    public string OldNoobFolderText => OldNoobFolder is not { } folder ? S["export.folder_no_install"]
        : Directory.Exists(folder) ? S.Format("export.folder_here", ("folder", folder))
        : S.Format("export.folder_absent", ("folder", folder));

    public string ExportDoneText => _exportedFile is { } file ? S.Format("export.done", ("file", file)) : string.Empty;

    public bool HasExportDone => _exportedFile is not null;

    public bool CanExportOldNoob => ExportLine is not null && Idle && _session is not null;

    /// <summary>The banner's button: the export screen.</summary>
    public void OpenOldNoobExport()
    {
        if (ExportLine is null) return;
        _exportedFile = null;
        SavesGo(SavesScreen.Export);
        ChangedExport();
    }

    /// <summary>"Export to a file…": the launcher's folder picker; the pick runs the export.</summary>
    public void PickOldNoobExportFolder()
    {
        if (!CanExportOldNoob) return;
        OpenPicker(new(S["export.pick_title"], PicksFolder: true, PicksFile: false, ExportOldNoobAsync)
        {
            Start = _exportLastFolder ?? Environment.GetFolderPath(Environment.SpecialFolder.MyDocuments),
            PickFolderLabel = S["export.pick_here"],
        });
    }

    /// <summary>Every folder an export never goes to: Azimuth's install (each game's saves live under it).</summary>
    private List<string> ExportForbidden()
    {
        var list = new List<string>();
        if (_install is { } install)
        {
            list.Add(install.Root);
            list.AddRange(GameDefinition.All.Select(g => AzimuthSaves.SavesRoot(install.Root, g)));
        }
        return list;
    }

    /// <summary>The export into <paramref name="folder"/>, under the busy overlay; the result (or why not) stays on the screen.</summary>
    internal async Task ExportOldNoobAsync(string folder)
    {
        if (ExportLine is not { } line || _session is not { } session) return;
        _exportLastFolder = folder;
        // checked before anything is asked of the server: Azimuth's folder never takes the file
        if (OldNoobExport.Refusal(folder, ExportForbidden()) is { } refused)
        {
            Log($"export: old Noob save of {line.CareerId} refused for the chosen folder ({refused})");
            Message = S[refused];
            return;
        }
        if (_session.Info.Services?.Saves is not { } origin || !Uri.TryCreate(origin, UriKind.Absolute, out var saves) || GameOf(line.Game) is not { } game)
        {
            Message = S["export.failed_unreachable"];
            return;
        }
        await Run(S["busy.exporting_old_save"], async () =>
        {
            OldNoobExportResult result;
            try
            {
                var api = new SavesApiClient(Http, saves, ClientHeader, session.CareerGame(line.Game), game.Saves.Bundle);
                var access = await session.Tokens.AccessAsync(CancellationToken.None);
                result = await OldNoobExport.ExportAsync(api, access, line, folder, ExportForbidden());
            }
            catch (Exception e) when (e is HttpRequestException or TaskCanceledException or ApiException)
            {
                Log("export: old Noob save not read: " + e.GetType().Name);
                Message = S["export.failed_unreachable"];
                return;
            }
            Log($"export: old Noob save of {line.CareerId} r{result.Revision} → {result.Outcome} {result.Code}");
            switch (result.Outcome)
            {
                case OldNoobExportOutcome.Written:
                    _exportedFile = result.File;
                    Message = S.Format("export.done", ("file", result.File ?? string.Empty));
                    break;
                case OldNoobExportOutcome.Refused:
                    Message = S[result.Code ?? OldNoobExport.RefusedKey];
                    break;
                case OldNoobExportOutcome.NoRevision:
                    Message = S["export.nothing"];
                    break;
                default:
                    Message = S.Format("export.failed", ("code", result.Code ?? "?"));
                    break;
            }
            ChangedExport();
        });
    }

    /// <summary>"Open the old folder": Explorer on the profile's old local folder (nothing in it is changed).</summary>
    public void OpenOldNoobFolder()
    {
        if (OldNoobFolder is not { } folder || !Directory.Exists(folder)) { Message = OldNoobFolderText; return; }
        try { System.Diagnostics.Process.Start(new System.Diagnostics.ProcessStartInfo("explorer.exe", "\"" + folder + "\"") { UseShellExecute = true })?.Dispose(); }
        catch (Exception e) when (e is System.ComponentModel.Win32Exception or InvalidOperationException) { Log("export: Explorer not opened: " + e.GetType().Name); }
    }

    private static GameDefinition? GameOf(string id) => GameDefinition.All.FirstOrDefault(g => g.Id == id);

    private static string GameName(string id) => GameOf(id)?.Name ?? id;

    private void ChangedExport()
    {
        Changed(nameof(ShowOldNoobBanner));
        Changed(nameof(ShowSavesExport));
        Changed(nameof(OldNoobBannerText));
        Changed(nameof(ExportLineText));
        Changed(nameof(OldNoobFolderText));
        Changed(nameof(OldNoobFolderExists));
        Changed(nameof(ExportDoneText));
        Changed(nameof(HasExportDone));
        Changed(nameof(CanExportOldNoob));
    }
}
