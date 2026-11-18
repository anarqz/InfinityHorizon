// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.ComponentModel;
using System.Globalization;
using System.Text.Json;
using Infinity.Core.Mods;

namespace Infinity.App;

/// <summary>One <c>choices[]</c> entry of an <c>enum</c> option: the token written to the INI and the label the form shows.</summary>
public sealed record ManifestChoice(string Value, string Label);

/// <summary>
/// One manifest option in the form. Numbers show in the player's culture; locked options are read-only with the manifest's reason.
/// An <c>enum</c> shows its choices' localized labels (English, then the token) and writes the token; a <c>folder</c> shows its path
/// (an older token as its folder under <paramref name="azimuthRoot"/>, the empty token as its <c>emptyLabel</c>) with a hint when it is not there now.
/// </summary>
/// <param name="modFolder">The mod's own absolute folder, which a chosen folder may not be or be inside.</param>
public sealed class ManifestOptionItem(JsonElement definition, JsonElement value, string locale, Action changed, string? azimuthRoot = null, string? modFolder = null) : INotifyPropertyChanged
{
    private readonly CultureInfo _culture = CultureInfo.GetCultureInfo(locale);
    private readonly SynchronizationContext? _ui = SynchronizationContext.Current;
    private int _folderCheck;
    private bool _folderThere = true;
    public string Id => definition.GetProperty("id").GetString()!;
    public string Label => Localized("label") ?? Id;
    public string Help => Localized("description") ?? "";
    public bool IsBoolean => definition.GetProperty("type").GetString() == "boolean";
    public bool IsChoice => definition.GetProperty("type").GetString() == "enum";
    public bool IsFolder => definition.GetProperty("type").GetString() == ModFolderOption.Type;
    public bool IsNumber => !IsBoolean && !IsChoice && !IsFolder;
    public bool Editable => !definition.TryGetProperty("lockedValue", out _);
    public string LockReason => Editable ? "" : Localized("lockReason") ?? "";
    public bool HasLockReason => LockReason.Length > 0;
    public CultureInfo Culture => _culture;
    public IReadOnlyList<ManifestChoice> Choices => IsChoice ? definition.GetProperty("choices").EnumerateArray()
        .Select(c => new ManifestChoice(c.GetProperty("value").GetString()!, LocalizedIn(c, "label") ?? c.GetProperty("value").GetString()!)).ToArray() : [];
    private string _text = ManifestIni.Display(definition.TryGetProperty("lockedValue", out var locked) ? locked : value, CultureInfo.GetCultureInfo(locale));
    public string Text
    {
        get => _text;
        set
        {
            if (_text == value) return;
            _text = value;
            foreach (var name in new[] { nameof(Text), nameof(Checked), nameof(FolderText), nameof(FolderPath), nameof(HasFolder) }) PropertyChanged?.Invoke(this, new(name));
            changed();
            if (IsFolder) _ = CheckFolder();
        }
    }
    public bool Checked { get => _text is "1" or "true"; set => Text = value ? "1" : "0"; }

    /// <summary>The folder the value names (an older token's under the Azimuth folder), or null for none.</summary>
    public string? FolderPath => IsFolder ? ModFolderOption.Resolve(definition, _text, azimuthRoot) : null;
    public bool HasFolder => FolderPath is not null;
    /// <summary>The read-only text of a folder option: its path, or its <c>emptyLabel</c>.</summary>
    public string FolderText => !IsFolder ? "" : FolderPath ?? (_text == ModFolderOption.EmptyValue(definition) ? Localized("emptyLabel") ?? _text : _text);
    /// <summary>A folder is named but is not there now (checked off the UI thread): the hint shows and Open hides.</summary>
    public bool FolderMissing => HasFolder && !_folderThere;
    public bool CanOpenFolder => HasFolder && _folderThere;
    /// <summary>Where Choose folder… starts: the current folder when it is there, else the player's Music folder.</summary>
    public string FolderStart => CanOpenFolder ? FolderPath! : Environment.GetFolderPath(Environment.SpecialFolder.MyMusic);

    /// <summary>A folder the picker returned: stored when it is valid for this option (false: the form says why, nothing changes).</summary>
    public bool ChooseFolder(string path)
    {
        if (!IsFolder || !Editable || !ModFolderOption.ValidPath(path, modFolder)) return false;
        Text = path;
        return true;
    }

    /// <summary>Clear: the option's empty token (no folder).</summary>
    public void ClearFolder() { if (IsFolder && Editable) Text = ModFolderOption.EmptyValue(definition); }

    /// <summary>Whether the named folder is there, off the UI thread (a UNC path of an absent server may take seconds).</summary>
    public async Task CheckFolder()
    {
        var check = Interlocked.Increment(ref _folderCheck);
        var path = FolderPath;
        var there = await Task.Run(() => path is null || System.IO.Directory.Exists(path)).ConfigureAwait(false);
        if (check != Volatile.Read(ref _folderCheck)) return; // a newer value is being checked
        // the answer lands on the thread that made the item (the window's), and the returned task ends after it
        var applied = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        void Apply()
        {
            if (check == Volatile.Read(ref _folderCheck))
            {
                _folderThere = there;
                PropertyChanged?.Invoke(this, new(nameof(FolderMissing))); PropertyChanged?.Invoke(this, new(nameof(CanOpenFolder)));
            }
            applied.SetResult();
        }
        if (_ui is null) Apply(); else _ui.Post(_ => Apply(), null);
        await applied.Task.ConfigureAwait(false);
    }

    public event PropertyChangedEventHandler? PropertyChanged;
    private string? Localized(string field) => LocalizedIn(definition, field);
    private string? LocalizedIn(JsonElement node, string field) => node.TryGetProperty(field, out var texts) && texts.ValueKind == JsonValueKind.Object
        ? (texts.TryGetProperty(locale, out var text) ? text : texts.GetProperty("en")).GetString()! : null;
}

/// <summary>A <c>config.openFolders</c> button of the settings panel.</summary>
public sealed record ModOpenFolderItem(ModOpenFolder Folder, string Label);
