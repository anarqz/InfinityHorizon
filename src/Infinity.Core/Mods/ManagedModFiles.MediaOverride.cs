// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Mods;

/// <summary>
/// One entry under the game's media overrides that a Supporter + Hardcore session may not have (OD-115): a game-relative path and
/// a stable reason: <c>unexpected</c> (a file), <c>link</c> (a reparse point) or <c>limit</c> (more entries than are checked).
/// </summary>
public sealed record MediaOverrideFinding(string Path, string Reason);

public sealed partial class ManagedModFiles
{
    /// <summary>
    /// OD-115: the game-root folder whose files the game reads in place of its own media (<c>mediaoverride/X</c> over <c>media/X</c>).
    /// No mod installs there since OD-232 (mods load through ExtraOptions' <c>mods\</c>); only the Hardcore check reads it.
    /// </summary>
    public const string MediaOverrideFolder = "mediaoverride";

    /// <summary>The most entries (files and folders) below the media overrides the Hardcore check walks; more is a finding.</summary>
    public const int MaxMediaOverrideEntries = 20000;

    /// <summary>
    /// OD-115, Supporter + Hardcore only (the caller applies <c>IntegrityPolicy</c>): the game reads every file under
    /// <c>&lt;game&gt;/mediaoverride</c> by itself, around the loader's mods (which Hardcore turns off), so any file there, or a link,
    /// is a finding (paths only). Since OD-232 no mod the launcher installs puts one there; a retired installation that did is removed
    /// (or disabled) on Mods, which restores the folder. Read-only. Not anti-cheat: the launcher refuses its own Hardcore launch
    /// (OD-111 compatibility evidence).
    /// </summary>
    public IReadOnlyList<MediaOverrideFinding> AuditMediaOverrides()
    {
        if (_game is null) return [];
        var findings = new List<MediaOverrideFinding>();
        var root = System.IO.Path.Combine(_game, MediaOverrideFolder);
        if (!Directory.Exists(root) && !File.Exists(root)) return findings;
        if (IsLink(root) || File.Exists(root)) return [new(MediaOverrideFolder, "link")];
        var pending = new Stack<string>(); pending.Push(root);
        var entries = 0;
        while (pending.Count > 0)
        {
            var folder = pending.Pop();
            foreach (var entry in Directory.EnumerateFileSystemEntries(folder))
            {
                var relative = MediaOverrideFolder + "/" + System.IO.Path.GetRelativePath(root, entry).Replace('\\', '/');
                if (++entries > MaxMediaOverrideEntries) { findings.Add(new(MediaOverrideFolder, "limit")); pending.Clear(); break; }
                if (IsLink(entry)) { findings.Add(new(relative, "link")); continue; }
                if (Directory.Exists(entry)) { pending.Push(entry); continue; }
                findings.Add(new(relative, "unexpected"));
            }
        }
        return findings.OrderBy(f => f.Path, StringComparer.OrdinalIgnoreCase).ThenBy(f => f.Reason, StringComparer.Ordinal).ToList();
    }
}
