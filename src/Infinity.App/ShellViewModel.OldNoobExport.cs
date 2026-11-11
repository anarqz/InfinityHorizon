// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Net.Http;
using Infinity.Core.Api;
using Infinity.Core.Process;
using Infinity.Core.Saves;

namespace Infinity.App;

/// <summary>
/// OD-239 items 6-7, OD-240 item 5: "Export to a file…" of an old Noob cloud line, from its game's AZIMUTH row ⋯ on Profiles
/// (ShellViewModel.OldSaves.cs). The launcher's own folder picker opens; the line's newest revision is downloaded (SAVES_API §4.2) and written
/// to a new file in the folder picked, never into Azimuth's folder. Which lines exist is read from the cached <c>/me</c> only
/// (<see cref="OldNoobExport.Lines"/>, no request).
/// </summary>
public sealed partial class ShellViewModel
{
    private string? _exportLastFolder;
    private OldNoobLine? _exportPicked;

    /// <summary>The account's old Noob lines, from the last <c>/me</c>.</summary>
    private IReadOnlyList<OldNoobLine> OldNoobLines => OfflineMode ? [] : OldNoobExport.Lines(_session?.Me);

    /// <summary>The line being exported: the one its AZIMUTH row's ⋯ picked (while <c>/me</c> still names it), else none.</summary>
    private OldNoobLine? ExportLine => _exportPicked is { } picked
        ? OldNoobLines.FirstOrDefault(l => l.CareerId == picked.CareerId && l.Game == picked.Game) : null;

    public bool CanExportOldNoob => Idle && _session is not null;

    /// <summary>"Export to a file…" of <paramref name="line"/>: the launcher's folder picker; the pick runs the export.</summary>
    internal void PickOldNoobExportFolder(OldNoobLine line)
    {
        ArgumentNullException.ThrowIfNull(line);
        _exportPicked = line;
        if (!CanExportOldNoob || ExportLine is null) return;
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

    /// <summary>The export into <paramref name="folder"/>, under the busy overlay; the result (or why not) stays under the Profiles cards.</summary>
    internal async Task ExportOldNoobAsync(string folder)
    {
        if (ExportLine is not { } line || _session is not { } session) return;
        _exportLastFolder = folder;
        // checked before anything is asked of the server: Azimuth's folder never takes the file
        if (OldNoobExport.Refusal(folder, ExportForbidden()) is { } refused)
        {
            Log($"export: old Noob save of {line.CareerId} refused for the chosen folder ({refused})");
            SetOldSaveResult(S[refused]);
            return;
        }
        if (_session.Info.Services?.Saves is not { } origin || !Uri.TryCreate(origin, UriKind.Absolute, out var saves) || GameOf(line.Game) is not { } game)
        {
            SetOldSaveResult(S["export.failed_unreachable"]);
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
                SetOldSaveResult(S["export.failed_unreachable"]);
                return;
            }
            Log($"export: old Noob save of {line.CareerId} r{result.Revision} → {result.Outcome} {result.Code}");
            SetOldSaveResult(result.Outcome switch
            {
                OldNoobExportOutcome.Written => S.Format("export.done", ("file", result.File ?? string.Empty)),
                OldNoobExportOutcome.Refused => S[result.Code ?? OldNoobExport.RefusedKey],
                OldNoobExportOutcome.NoRevision => S["export.nothing"],
                _ => S.Format("export.failed", ("code", result.Code ?? "?")),
            });
        });
    }

    private static GameDefinition? GameOf(string id) => GameDefinition.All.FirstOrDefault(g => g.Id == id);

    private static string GameName(string id) => GameOf(id)?.Name ?? id;
}
