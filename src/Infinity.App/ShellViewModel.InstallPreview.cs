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

    private IReadOnlyList<string> PreviewFolders
    {
        get
        {
            if (_selectedMod?.Manifest is not { } mod) return [];
            var folders = new List<string>();
            if (mod.Files.Any(f => f.Root == "azimuth") && _install?.Root is { } azimuth) folders.Add(azimuth);
            if (mod.Files.Any(f => f.Root == "game") && ContentLocation?.Mount is { } mount) folders.Add(mount);
            return folders;
        }
    }

    /// <summary>The existing files the install would replace (the game's or another installation's), backed up and restored later.</summary>
    private IReadOnlyList<string> PreviewReplaced
    {
        get
        {
            if (!SelectedModInstallPreview || ModManagerFactory() is not { } factory) return [];
            var mod = _selectedMod!.Manifest;
            var key = mod.Digest + "|" + ContentLocation?.Mount + "|" + _install?.Root;
            if (_previewReplaced?.Key != key)
                try { _previewReplaced = (key, factory().ExistingFilesReplaced(mod)); }
                catch (Exception error) when (ManagedModFiles.IsStateError(error)) { _previewReplaced = (key, []); }
            return _previewReplaced.Value.Files;
        }
    }

    public string InstallPreviewText
    {
        get
        {
            if (!SelectedModInstallPreview) return "";
            if (InstallPreviewNeedsGameFolder) return S["mods.error.game_folder"];
            var path = string.Join(" · ", PreviewFolders);
            var count = PreviewReplaced.Count;
            return count > 0 ? S.Format("mods.preview_replaces", ("path", path), ("count", count.ToString(CultureInfo.CurrentCulture)))
                : S.Format("mods.preview_into", ("path", path));
        }
    }

    public string InstallPreviewFiles => PreviewReplaced is { Count: > 0 } files ? string.Join("\n", files.Take(8)) + (files.Count > 8 ? "\n…" : "") : "";

    public bool InstallPreviewHasFiles => InstallPreviewFiles.Length > 0;

    public string InstallPreviewLabel => PreviewReplaced.Count is var count and > 0
        ? S.Format("mods.install_replacing", ("count", count.ToString(CultureInfo.CurrentCulture))) : S["mods.install"];

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
