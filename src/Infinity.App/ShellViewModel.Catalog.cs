// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>
/// OD-116 "Use a different catalog": a player-chosen catalog address instead of the official file on the launcher repository.
/// Everything it lists may be installed, native mods and locked settings included, always under the unofficial/unverified warning
/// (Configure, the Mods page and each of its cards).
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _customCatalogRead;
    private Uri? _customCatalog;
    private string _catalogDraft = "";

    /// <summary>The saved custom address, read once from the state folder; null: the official catalog.</summary>
    private Uri? CustomCatalog
    {
        get
        {
            if (!_customCatalogRead) { _customCatalog = DevelopmentMods ? null : ModCatalogSetting.Load(_stateDirectory); _customCatalogRead = true; }
            return _customCatalog;
        }
    }

    public bool UsingCustomCatalog => !DevelopmentMods && CustomCatalog is not null;
    public string CatalogSourceText => UsingCustomCatalog ? S.Format("catalog.using_custom", ("url", CustomCatalog!.AbsoluteUri)) : S["catalog.using_official"];
    public string CustomCatalogWarning => UsingCustomCatalog ? S.Format("catalog.unofficial_warning", ("url", CustomCatalog!.AbsoluteUri)) : "";

    /// <summary>The address field (Configure); Steam keyboard on focus like every text field.</summary>
    public string CatalogDraft
    {
        get => _catalogDraft;
        set { _catalogDraft = value ?? ""; Changed(); Changed(nameof(CanUseCustomCatalog)); Changed(nameof(CatalogDraftInvalid)); }
    }
    private Uri? DraftUri => ModCatalogSetting.Valid(_catalogDraft);
    public bool CatalogDraftInvalid => _catalogDraft.Trim().Length > 0 && DraftUri is null;
    public bool CanUseCustomCatalog => ModsIdle && !DevelopmentMods && DraftUri is { } url && !OfficialModCatalog.IsOfficial(url) && url != CustomCatalog;
    public bool CanUseOfficialCatalog => ModsIdle && UsingCustomCatalog;

    public Task UseCustomCatalogAsync() => CanUseCustomCatalog ? SwitchCatalogAsync(DraftUri) : Task.CompletedTask;
    public Task UseOfficialCatalogAsync() => CanUseOfficialCatalog ? SwitchCatalogAsync(null) : Task.CompletedTask;

    /// <summary>Saves the choice, forgets the loaded catalog (each address keeps its own cache) and loads the chosen one.</summary>
    private async Task SwitchCatalogAsync(Uri? url)
    {
        try { ModCatalogSetting.Save(_stateDirectory, url); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or ModException)
        {
            Log("mods: catalog choice not saved: " + error.GetType().Name);
            Message = S["catalog.save_failed"];
            return;
        }
        Log("mods: catalog " + (url is null ? "official" : "custom"));
        _customCatalog = url; _customCatalogRead = true; _catalogDraft = "";
        _catalog = null; _manifestCatalog = []; _catalogLoaded = false; _catalogFailure = null;
        Changed(string.Empty);
        await LoadModCatalogAsync();
        if (_catalogFailure is null) Message = url is null ? S["catalog.switched_official"] : S["catalog.switched_custom"];
    }
}
