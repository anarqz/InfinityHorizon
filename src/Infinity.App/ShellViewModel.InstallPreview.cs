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

    /// <summary>How many previews are kept (the page shows two at most: the pane's and the confirmation's).</summary>
    private const int MaxPreviews = 16;

    // T-1118: one entry per mod and folder. 3.1.1 to 3.2.0 kept a single preview, and the page can show two at once (the information
    // pane's mod and the confirmation's): each read ended with a notify-all, the other preview was then missing and read again, without
    // end (hundreds of notify-alls a second, a mod manager built for each, the window's thread never free again, memory by the gigabyte).
    private readonly Dictionary<string, IReadOnlyList<string>> _previewReplaced = new(StringComparer.Ordinal);
    private readonly Dictionary<string, Task<IReadOnlyList<string>>> _previewReading = new(StringComparer.Ordinal);
    /// <summary>The previews whose read failed (the reason shown in place of the count); known, so nothing reads them again in a loop.</summary>
    private readonly Dictionary<string, string> _previewFailed = new(StringComparer.Ordinal);
    private int _previewGeneration;

    /// <summary>Test seam: the preview reads started since the start.</summary>
    internal int PreviewReads { get; private set; }

    private string PreviewKey(ModManifest mod) => mod.Digest + "|" + _install?.Root;

    /// <summary>
    /// What is there now, never an earlier preview's: every preview is read again when next shown (a read in flight is dropped). Called
    /// when a card's button acts, when a mod operation ends (it changed the files another mod's preview counted) and when the window
    /// comes back to the front (the player may have changed the game's files meanwhile). True when a preview was known.
    /// </summary>
    internal bool ForgetPreviews()
    {
        var known = _previewReplaced.Count > 0;
        _previewReplaced.Clear();
        _previewFailed.Clear();
        _previewGeneration++;
        return known;
    }

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
        if (_previewReplaced.TryGetValue(PreviewKey(mod), out var files)) return files;
        _ = ReplacedByAsync(mod);
        return [];
    }

    /// <summary>The preview of <paramref name="mod"/> is read (the pane's Install and the card's confirmation wait for it).</summary>
    private bool PreviewKnown(ModManifest mod) => _previewReplaced.ContainsKey(PreviewKey(mod));

    /// <summary>
    /// The existing files <paramref name="mod"/> would replace, read on a worker. One read per mod and folder at a time, shared by
    /// every caller (the bindings that ask during one notify-all, a card's button), and one notify-all when it is known.
    /// </summary>
    internal Task<IReadOnlyList<string>> ReplacedByAsync(ModManifest mod)
    {
        ArgumentNullException.ThrowIfNull(mod);
        if (ModManagerFactory() is not { } factory) return Task.FromResult<IReadOnlyList<string>>([]);
        var key = PreviewKey(mod);
        if (_previewReplaced.TryGetValue(key, out var known)) return Task.FromResult(known);
        // a read started before the previews were forgotten is not this one: its slot carries the generation
        var generation = _previewGeneration;
        var slot = generation.ToString(CultureInfo.InvariantCulture) + ":" + key;
        if (_previewReading.TryGetValue(slot, out var reading)) return reading;
        // a failure of the read (the mod manager's state errors included) is caught by ReadPreviewAsync: shown with its reason, never
        // passed off as "replaces nothing"
        return _previewReading[slot] = ReadPreviewAsync(key, slot, generation,
            () => PreviewReadForTests is { } fake ? fake(mod) : factory().ExistingFilesReplaced(mod));
    }

    /// <summary>Test seam: the read of the files a mod would replace (in place of the mod manager's).</summary>
    internal Func<ModManifest, IReadOnlyList<string>>? PreviewReadForTests { get; set; }

    private async Task<IReadOnlyList<string>> ReadPreviewAsync(string key, string slot, int generation, Func<IReadOnlyList<string>> read)
    {
        // never inside the caller (a binding's getter): the read is registered first, and its notify-all is raised from the dispatcher
        await Task.Yield();
        PreviewReads++;
        IReadOnlyList<string> files = [];
        string? failure = null;
        try { files = await Task.Run(read); }
        // a read that fails (a file in use, a folder that cannot be listed) is an answer too: logged, shown with its reason, and not
        // read again until the previews are forgotten (never an unobserved task, never a loop)
        catch (Exception error) when (!IsFatal(error))
        {
            failure = error.GetType().Name;
            Log("mods: the files a mod would replace could not be read (" + failure + ")");
        }
        finally { _previewReading.Remove(slot); }
        if (generation != _previewGeneration) return files; // forgotten meanwhile: whoever shows it next reads what is there then
        if (_previewReplaced.Count >= MaxPreviews) { _previewReplaced.Clear(); _previewFailed.Clear(); }
        _previewReplaced[key] = files;
        if (failure is not null) _previewFailed[key] = failure;
        Changed(string.Empty);
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
        if (_previewFailed.TryGetValue(PreviewKey(mod), out var reason)) return S.Format("mods.preview_unreadable", ("path", path), ("reason", reason));
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
    // the same small confirmation asks before Enable all / Disable all (ShellViewModel.ModsToggleAll.cs)
    public bool ShowModConfirm => (_modConfirm is not null || _toggleAllConfirm is not null) && ShowMods && !ModOperationBusy;
    public bool ModConfirmIsUpdate => _modConfirm is not null && _modConfirmUpdate;
    private ModManifest? ModConfirmTarget => _modConfirm is not { } card ? null : _modConfirmUpdate ? card.Next : card.Manifest;
    public string ModConfirmTitle => _toggleAllConfirm is not null ? ToggleAllTitle
        : _modConfirm is not { } card ? "" : S.Format(_modConfirmUpdate ? "mods.confirm_update_title" : "mods.confirm_install_title", ("mod", card.Name));
    public string ModConfirmText => _toggleAllConfirm is not null ? ToggleAllText : _modConfirm is not { } card || ModConfirmTarget is not { } target ? ""
        : _modConfirmUpdate ? card.UpdateSummary + "\n" + PreviewTextFor(target) : PreviewTextFor(target);
    public string ModConfirmFiles => _toggleAllConfirm is null && ModConfirmTarget is { } target && PreviewKnown(target) && ReplacedBy(target) is { Count: > 0 } files ? FileList(files) : "";
    public bool ModConfirmHasFiles => ModConfirmFiles.Length > 0;
    public string ModConfirmLabel => _toggleAllConfirm is not null ? ToggleAllLabel : ModConfirmTarget is not { } target ? "" : ReplacedBy(target).Count is var count and > 0
        ? Counted(_modConfirmUpdate ? "mods.update_replacing" : "mods.install_replacing", count) : S[_modConfirmUpdate ? "mods.confirm_update" : "mods.install"];
    public bool CanConfirmModCard => _toggleAllConfirm is { } enable ? (enable ? CanEnableAllMods : CanDisableAllMods)
        : _modConfirm is { CanAct: true } && ModsIdle && ModConfirmTarget is { } target && PreviewKnown(target);

    private void OpenModConfirm(ModCard card, bool update)
    {
        _modConfirm = card; _modConfirmUpdate = update; _toggleAllConfirm = null;
        Changed(string.Empty);
    }

    public void CancelModConfirm()
    {
        if (_modConfirm is null && _toggleAllConfirm is null) return;
        _modConfirm = null; _modConfirmUpdate = false; _toggleAllConfirm = null;
        Changed(string.Empty);
    }

    /// <summary>The small confirmation's button: the install (or update) the player has now seen, on that card (or Enable/Disable all).</summary>
    public async Task ConfirmModCardAsync()
    {
        if (!CanConfirmModCard) return;
        if (_toggleAllConfirm is not null) { await ConfirmToggleAllModsAsync(); return; }
        if (_modConfirm is not { } card) return;
        var update = _modConfirmUpdate;
        _modConfirm = null; _modConfirmUpdate = false; Changed(string.Empty);
        if (update) { await UpdateModAsync(card); return; }
        _installConfirmedDigest = card.Manifest.Digest;
        try { await ActOnModAsync(card); }
        finally { _installConfirmedDigest = null; Changed(string.Empty); }
    }
}
