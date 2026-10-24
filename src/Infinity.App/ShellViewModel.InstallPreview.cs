// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>
/// OD-116/117 (Alex, 1.0.11): every mod shows where it installs and which existing files it replaces before anything is written, not
/// only mods made of the player's own files. Install on a card opens the information pane with this preview; its button installs.
/// </summary>
public sealed partial class ShellViewModel
{
    private string? _installPreviewDigest, _installConfirmedDigest;
    private (string Key, IReadOnlyList<string> Files)? _previewReplaced;

    /// <summary>A card's Install that writes files (not an adoption, not a player's-files mod: those have their own pane).</summary>
    private bool NeedsInstallPreview(ModCard card) => !card.Installed && !card.Manifest.IsExternal && card.Action != S["mods.adopt"]
        && _installConfirmedDigest != card.Manifest.Digest;

    public bool SelectedModInstallPreview => _selectedMod is { Installed: false } card && _installPreviewDigest == card.Manifest.Digest;

    /// <summary>The mod writes game files and no game folder is set: the preview says so and Install waits.</summary>
    public bool InstallPreviewNeedsGameFolder => SelectedModInstallPreview && ContentLocation is null && _selectedMod!.Manifest.Files.Any(f => f.Root == "game");

    /// <summary>The folders <paramref name="mod"/> writes into (Azimuth's and the game's).</summary>
    private List<string> FoldersFor(ModManifest mod)
    {
        var folders = new List<string>();
        if (mod.Files.Any(f => f.Root == "azimuth") && _install?.Root is { } azimuth) folders.Add(azimuth);
        if (mod.Files.Any(f => f.Root == "game") && ContentLocation?.Mount is { } mount) folders.Add(mount);
        return folders;
    }

    /// <summary>
    /// The existing files <paramref name="mod"/> would replace (the game's or another installation's; not its own installed ones, so an
    /// update counts only what it newly covers), backed up and restored later.
    /// </summary>
    private IReadOnlyList<string> ReplacedBy(ModManifest mod)
    {
        if (ModManagerFactory() is not { } factory) return [];
        var key = mod.Digest + "|" + ContentLocation?.Mount + "|" + _install?.Root;
        if (_previewReplaced?.Key != key)
            try { _previewReplaced = (key, factory().ExistingFilesReplaced(mod)); }
            catch (Exception error) when (ManagedModFiles.IsStateError(error)) { _previewReplaced = (key, []); }
        return _previewReplaced.Value.Files;
    }

    /// <summary>A counted string: the <c>_one</c> form for exactly one ("Replace 1 file"), the plural otherwise.</summary>
    private string Counted(string key, int count, params (string Name, string Value)[] more) =>
        S.Format(count == 1 ? key + "_one" : key, [("count", count.ToString(CultureInfo.CurrentCulture)), .. more]);

    /// <summary>Alex (1.0.11): "Installs into &lt;folder&gt; · replaces N files (backed up, restored on disable or uninstall)".</summary>
    private string PreviewTextFor(ModManifest mod)
    {
        var path = string.Join(" · ", FoldersFor(mod));
        var count = ReplacedBy(mod).Count;
        return count > 0 ? Counted("mods.preview_replaces", count, ("path", path)) : S.Format("mods.preview_into", ("path", path));
    }

    private static string FileList(IReadOnlyList<string> files) => string.Join("\n", files.Take(8)) + (files.Count > 8 ? "\n…" : "");

    private IReadOnlyList<string> PreviewReplaced => SelectedModInstallPreview ? ReplacedBy(_selectedMod!.Manifest) : [];

    public string InstallPreviewText => !SelectedModInstallPreview ? ""
        : InstallPreviewNeedsGameFolder ? S["mods.error.game_folder"] : PreviewTextFor(_selectedMod!.Manifest);

    public string InstallPreviewFiles => PreviewReplaced is { Count: > 0 } files ? FileList(files) : "";

    public bool InstallPreviewHasFiles => InstallPreviewFiles.Length > 0;

    public string InstallPreviewLabel => PreviewReplaced.Count is var count and > 0 ? Counted("mods.install_replacing", count) : S["mods.install"];

    // A reviewed update (review, T2 M7): the same preview for the files the new version newly covers, before its confirmation.
    private ModManifest? UpdatePreviewManifest => _selectedMod is { UpdateAvailable: true, Next: { IsExternal: false } next } ? next : null;

    public string UpdatePreviewText => UpdatePreviewManifest is not { } next ? ""
        : ContentLocation is null && next.Files.Any(f => f.Root == "game") ? S["mods.error.game_folder"] : PreviewTextFor(next);

    public string UpdatePreviewFiles => UpdatePreviewManifest is { } next && ReplacedBy(next) is { Count: > 0 } files ? FileList(files) : "";

    public bool UpdatePreviewHasFiles => UpdatePreviewFiles.Length > 0;

    public string ConfirmUpdateLabel => UpdatePreviewManifest is { } next && ReplacedBy(next).Count is var count and > 0
        ? Counted("mods.update_replacing", count) : S["mods.confirm_update"];

    public bool CanConfirmInstall => SelectedModInstallPreview && ModsIdle && _selectedMod!.CanAct && !InstallPreviewNeedsGameFolder;

    /// <summary>Opens the information pane with the install preview (the card's Install).</summary>
    private void ShowInstallPreview(ModCard card)
    {
        _installPreviewDigest = card.Manifest.Digest; _previewReplaced = null;
        ShowModInformation(card);
    }

    /// <summary>The preview's button: the install the player has now seen.</summary>
    public async Task ConfirmInstallAsync()
    {
        if (!CanConfirmInstall || _selectedMod is not { } card) return;
        _installConfirmedDigest = card.Manifest.Digest;
        try { await ActOnModAsync(card); }
        finally { _installConfirmedDigest = null; _installPreviewDigest = null; Changed(string.Empty); }
    }
}
