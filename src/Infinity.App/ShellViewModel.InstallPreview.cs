// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>
/// OD-116/117 (Alex, 1.0.11): every mod shows where it installs and which existing files it replaces before anything is written, not
/// only mods made of the player's own files. The information pane of a mod not installed shows this preview with its Install; a card's
/// Install installs at once, or asks first in a small confirmation when it replaces files already there (owner 2026-11-10, build #3).
/// </summary>
public sealed partial class ShellViewModel
{
    private string? _installPreviewDigest, _installConfirmedDigest;
    private (string Key, IReadOnlyList<string> Files)? _previewReplaced;

    /// <summary>A card's Install that writes files (not an adoption, not a player's-files mod: those have their own pane).</summary>
    private bool NeedsInstallPreview(ModCard card) => !card.Installed && !card.Manifest.IsExternal && card.Action != S["mods.adopt"]
        && _installConfirmedDigest != card.Manifest.Digest;

    public bool SelectedModInstallPreview => _selectedMod is { Installed: false } card && _installPreviewDigest == card.Manifest.Digest;

    /// <summary>
    /// The folder <paramref name="mod"/> installs into: its own ExtraOptions folder (OD-232) in its game's mods folder (OD-233),
    /// <c>&lt;Azimuth&gt;\mods\&lt;game folder&gt;\&lt;id&gt;</c>, whichever layout its catalog entry was written for (the same folder the install
    /// and the move to the per-game layout use, <see cref="ModManifest.ModsGame"/>).
    /// </summary>
    private string FolderFor(ModManifest mod) => _install?.Root is { } azimuth
        ? System.IO.Path.Combine(ExtraOptionsMods.ModsDirectory(azimuth, mod.ModsGame), ExtraOptionsMods.FolderOf(mod) ?? mod.Id) : "";

    /// <summary>
    /// The existing files <paramref name="mod"/> would replace (a copy placed by hand; not its own installed ones, so an update counts
    /// only what it newly covers), backed up and restored later.
    /// </summary>
    // owner 2026-11-10 ("the launcher never looks frozen"): read off the UI thread; the pane shows it when it is known
    private IReadOnlyList<string> ReplacedBy(ModManifest mod)
    {
        var key = mod.Digest + "|" + _install?.Root;
        if (_previewReplaced?.Key == key) return _previewReplaced.Value.Files;
        _ = ReplacedByAsync(mod);
        return [];
    }

    /// <summary>The preview of <paramref name="mod"/> is read (the pane's Install and the card's confirmation wait for it).</summary>
    private bool PreviewKnown(ModManifest mod) => _previewReplaced?.Key == mod.Digest + "|" + _install?.Root;

    private (string Key, Task<IReadOnlyList<string>> Read)? _previewReading;

    /// <summary>The existing files <paramref name="mod"/> would replace, read on a worker (one read per mod and folder at a time).</summary>
    internal async Task<IReadOnlyList<string>> ReplacedByAsync(ModManifest mod)
    {
        ArgumentNullException.ThrowIfNull(mod);
        if (ModManagerFactory() is not { } factory) return [];
        var key = mod.Digest + "|" + _install?.Root;
        if (_previewReplaced?.Key == key) return _previewReplaced.Value.Files;
        if (_previewReading is not { } reading || reading.Key != key)
        {
            reading = (key, Task.Run<IReadOnlyList<string>>(() =>
            {
                try { return factory().ExistingFilesReplaced(mod); }
                catch (Exception error) when (ManagedModFiles.IsStateError(error)) { return []; }
            }));
            _previewReading = reading;
        }
        var files = await reading.Read;
        if (_previewReading?.Key == key) _previewReading = null;
        if (key == mod.Digest + "|" + _install?.Root) { _previewReplaced = (key, files); Changed(string.Empty); }
        return files;
    }

    /// <summary>A counted string: the <c>_one</c> form for exactly one ("Replace 1 file"), the plural otherwise.</summary>
    private string Counted(string key, int count, params (string Name, string Value)[] more) =>
        S.Format(count == 1 ? key + "_one" : key, [("count", count.ToString(CultureInfo.CurrentCulture)), .. more]);

    /// <summary>Alex (1.0.11): "Installs into &lt;folder&gt; · replaces N files (backed up, restored on disable or uninstall)".</summary>
    private string PreviewTextFor(ModManifest mod)
    {
        var path = FolderFor(mod);
        var count = ReplacedBy(mod).Count;
        return count > 0 ? Counted("mods.preview_replaces", count, ("path", path)) : S.Format("mods.preview_into", ("path", path));
    }

    private static string FileList(IReadOnlyList<string> files) => string.Join("\n", files.Take(8)) + (files.Count > 8 ? "\n…" : "");

    private IReadOnlyList<string> PreviewReplaced => SelectedModInstallPreview ? ReplacedBy(_selectedMod!.Manifest) : [];

    public string InstallPreviewText => !SelectedModInstallPreview ? "" : PreviewTextFor(_selectedMod!.Manifest);

    public string InstallPreviewFiles => PreviewReplaced is { Count: > 0 } files ? FileList(files) : "";

    public bool InstallPreviewHasFiles => InstallPreviewFiles.Length > 0;

    public string InstallPreviewLabel => PreviewReplaced.Count is var count and > 0 ? Counted("mods.install_replacing", count) : S["mods.install"];

    // A reviewed update (review, T2 M7): the same preview for the files the new version newly covers, before its confirmation.
    private ModManifest? UpdatePreviewManifest => _selectedMod is { UpdateAvailable: true, Next: { IsExternal: false } next } ? next : null;

    public string UpdatePreviewText => UpdatePreviewManifest is not { } next ? "" : PreviewTextFor(next);

    public string UpdatePreviewFiles => UpdatePreviewManifest is { } next && ReplacedBy(next) is { Count: > 0 } files ? FileList(files) : "";

    public bool UpdatePreviewHasFiles => UpdatePreviewFiles.Length > 0;

    public string ConfirmUpdateLabel => UpdatePreviewManifest is { } next && ReplacedBy(next).Count is var count and > 0
        ? Counted("mods.update_replacing", count) : S["mods.confirm_update"];

    public bool CanConfirmInstall => SelectedModInstallPreview && ModsIdle && _selectedMod!.CanAct && PreviewKnown(_selectedMod.Manifest);

    /// <summary>The information pane's Install: the install the player has now seen.</summary>
    public async Task ConfirmInstallAsync()
    {
        if (!CanConfirmInstall || _selectedMod is not { } card) return;
        _installConfirmedDigest = card.Manifest.Digest;
        try { await ActOnModAsync(card); }
        finally { _installConfirmedDigest = null; _installPreviewDigest = null; Changed(string.Empty); }
    }

    // Owner 2026-11-10 (3.1.1 build #3: "Clicking on the INSTALL button from the MAIN mod card, actually do not install, it opens the side
    // bar which then have the INSTALL button"): a card's button does its action. Only what the player must see first asks, in a small
    // confirmation over the page (never the side pane): an install that replaces files already there, and a reviewed update (M5).

    private ModCard? _modConfirm;
    private bool _modConfirmUpdate;

    /// <summary>The card whose Install or Update waits for the small confirmation, or null.</summary>
    public ModCard? ModConfirmCard => _modConfirm;
    public bool ShowModConfirm => _modConfirm is not null && ShowMods && !ModOperationBusy;
    public bool ModConfirmIsUpdate => _modConfirm is not null && _modConfirmUpdate;
    private ModManifest? ModConfirmTarget => _modConfirm is not { } card ? null : _modConfirmUpdate ? card.Next : card.Manifest;
    public string ModConfirmTitle => _modConfirm is not { } card ? "" : S.Format(_modConfirmUpdate ? "mods.confirm_update_title" : "mods.confirm_install_title", ("mod", card.Name));
    public string ModConfirmText => _modConfirm is not { } card || ModConfirmTarget is not { } target ? ""
        : _modConfirmUpdate ? card.UpdateSummary + "\n" + PreviewTextFor(target) : PreviewTextFor(target);
    public string ModConfirmFiles => ModConfirmTarget is { } target && PreviewKnown(target) && ReplacedBy(target) is { Count: > 0 } files ? FileList(files) : "";
    public bool ModConfirmHasFiles => ModConfirmFiles.Length > 0;
    public string ModConfirmLabel => ModConfirmTarget is not { } target ? "" : ReplacedBy(target).Count is var count and > 0
        ? Counted(_modConfirmUpdate ? "mods.update_replacing" : "mods.install_replacing", count) : S[_modConfirmUpdate ? "mods.confirm_update" : "mods.install"];
    public bool CanConfirmModCard => _modConfirm is { CanAct: true } && ModsIdle && ModConfirmTarget is { } target && PreviewKnown(target);

    private void OpenModConfirm(ModCard card, bool update)
    {
        _modConfirm = card; _modConfirmUpdate = update;
        Changed(string.Empty);
    }

    public void CancelModConfirm()
    {
        if (_modConfirm is null) return;
        _modConfirm = null; _modConfirmUpdate = false;
        Changed(string.Empty);
    }

    /// <summary>The small confirmation's button: the install (or update) the player has now seen, on that card.</summary>
    public async Task ConfirmModCardAsync()
    {
        if (!CanConfirmModCard || _modConfirm is not { } card) return;
        var update = _modConfirmUpdate;
        _modConfirm = null; _modConfirmUpdate = false; Changed(string.Empty);
        if (update) { await UpdateModAsync(card); return; }
        _installConfirmedDigest = card.Manifest.Digest;
        try { await ActOnModAsync(card); }
        finally { _installConfirmedDigest = null; Changed(string.Empty); }
    }
}
