// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.IO;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>
/// Alex 2026-11-10: "We need a button for ENABLING / DISABLING ALL mods from a game (which should remove or add the `#` character on each
/// line)". Enable all / Disable all under the Mods page's game chips, for the chosen game only, after the small confirmation over the page
/// ("Disable all 45 mods of Forza Horizon 2?"): one bulk operation under the screen lock (<see cref="ManagedModFiles.SetEnabledAll"/>: the
/// launcher's mods through their receipts, the hand-copied ones by their <c>#</c>, the Commons kept first), then one read of the mods.
/// Hardcore's rules still apply at PLAY.
/// </summary>
public sealed partial class ShellViewModel
{
    /// <summary>The chosen game's mods that are on (<c>true</c>) or off (<c>false</c>): the launcher's (not damaged) and the hand-copied ones.</summary>
    private int ModsOfGame(bool enabled)
    {
        var game = CurrentGame.Id;
        var managed = _modHealth.Installed.Count(i => i.Problem is null && i.Manifest is { } m && m.ModsGame == game && i.Enabled == enabled && (enabled || !m.Retired));
        return managed + ExtraOptionsModItems.Count(i => i.Game == game && i.Enabled == enabled);
    }

    /// <summary>The buttons show where the chosen game has a mods folder and at least one mod.</summary>
    public bool ShowToggleAllMods => _install is not null && ExtraOptionsLayout.HasModsFolder(CurrentGame.Id) && ModsOfGame(true) + ModsOfGame(false) > 0;
    public bool CanEnableAllMods => ModsIdle && !ModRecoveryRequired && ModsOfGame(false) > 0;
    public bool CanDisableAllMods => ModsIdle && !ModRecoveryRequired && ModsOfGame(true) > 0;

    /// <summary>The confirmation asked for: <c>true</c> Enable all, <c>false</c> Disable all, null none.</summary>
    private bool? _toggleAllConfirm;
    private int _toggleAllCount;

    /// <summary>A button's press: the small confirmation names the game and how many mods change.</summary>
    public void AskToggleAllMods(bool enable)
    {
        if (!(enable ? CanEnableAllMods : CanDisableAllMods)) { if (!ModsIdle) Message = S[ModsWaitReason]; return; }
        _modConfirm = null; _modConfirmUpdate = false;
        _toggleAllConfirm = enable; _toggleAllCount = ModsOfGame(!enable);
        Changed(string.Empty);
    }

    private string ToggleAllTitle => _toggleAllConfirm is not { } enable ? ""
        : S.Format(enable ? "mods.enable_all_title" : "mods.disable_all_title", ("count", _toggleAllCount.ToString(CultureInfo.CurrentCulture)), ("game", GameLabel));
    private string ToggleAllText => _toggleAllConfirm is not { } enable ? ""
        : S.Format(enable ? "mods.enable_all_text" : "mods.disable_all_text", ("game", GameLabel)) + (enable && ProfileHardcore ? "\n" + S["mods.toggle_all_hardcore"] : "");
    private string ToggleAllLabel => _toggleAllConfirm is not { } enable ? "" : S[enable ? "mods.enable_all" : "mods.disable_all"];

    /// <summary>The confirmation's button: every mod of the chosen game on (or off), in one operation.</summary>
    private async Task ConfirmToggleAllModsAsync()
    {
        if (_toggleAllConfirm is not { } enable) return;
        _toggleAllConfirm = null; Changed(string.Empty);
        await ToggleAllModsAsync(enable);
    }

    internal async Task ToggleAllModsAsync(bool enable)
    {
        if (_install is not { } install || ModManagerFactory() is not { } factory || !ExtraOptionsLayout.HasModsFolder(CurrentGame.Id)) return;
        var game = CurrentGame.Id; var hardcore = ProfileHardcore; var first = LoadFirstFolders(game);
        var names = _modHealth.Installed.Where(i => i.Manifest is not null).GroupBy(i => i.Id, StringComparer.Ordinal)
            .ToDictionary(g => g.Key, g => ModName(g.First().Manifest!), StringComparer.Ordinal);
        ModToggleAllResult? result = null;
        await RunModOperationAsync(async () =>
        {
            var progress = new Progress<string>(stage => ModStep(S["mods.progress." + stage]));
            // review 3.1.2: what it changes leaves the Hardcore holds inside the operation, under its lock (the factory's held files); a mod it
            // refuses, or a run that stops, keeps its hold
            result = await Task.Run(() => factory().SetEnabledAll(game, enable, first, progress, hardcore));
        }, S["mods.action_done"], label: S.Format(enable ? "mods.lock.enable_all" : "mods.lock.disable_all", ("game", GameLabel)));
        if (result is null) return; // refused or failed as a whole: RunModOperationAsync said why
        Log("mods: " + (enable ? "enable" : "disable") + " all " + game + ": " + result.Changed.Count + " managed, " + result.ManualChanged + " by hand"
            + (result.Failed.Count > 0 ? "; kept: " + string.Join(", ", result.Failed.Select(f => f.Id + " (" + f.Code + ")")) : ""));
        var count = (result.Changed.Count + result.ManualChanged).ToString(CultureInfo.CurrentCulture);
        Message = result.Failed.Count == 0 ? S.Format("mods.toggle_all_done", ("count", count))
            : S.Format("mods.toggle_all_partial", ("count", count), ("mods", string.Join(", ", result.Failed.Select(f => names.GetValueOrDefault(f.Id) ?? f.Id))));
    }
}
