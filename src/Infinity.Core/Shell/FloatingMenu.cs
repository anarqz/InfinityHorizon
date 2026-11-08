// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>
/// Owner 2026-11-08 (image 7): which ⋯ menu of the Profiles page is open (a profile card's or a game row's), one at a time. The menu floats
/// over the content (never pushes the rows down) and closes on an outside click, Esc, or picking an item.
/// </summary>
public sealed class FloatingMenuState
{
    /// <summary>The open menu's key (a profile id, or <c>profile|game</c> for a row), or null.</summary>
    public string? OpenKey { get; private set; }

    public bool IsOpen(string key) => OpenKey is not null && string.Equals(OpenKey, key, StringComparison.Ordinal);

    /// <summary>The ⋯ button: opens <paramref name="key"/>'s menu (closing any other), or closes it when it is the open one. True: now open.</summary>
    public bool Toggle(string key)
    {
        ArgumentException.ThrowIfNullOrEmpty(key);
        OpenKey = IsOpen(key) ? null : key;
        return OpenKey is not null;
    }

    /// <summary>Closes the open menu. True when one was open.</summary>
    public bool Close()
    {
        var was = OpenKey is not null;
        OpenKey = null;
        return was;
    }
}

/// <summary>Where a floating menu goes: its top-left corner, and whether it opened above its button.</summary>
public readonly record struct MenuSpot(double Left, double Top, bool Up);

/// <summary>Places a floating menu by its ⋯ button inside the window (pure arithmetic; the App measures).</summary>
public static class MenuPlacement
{
    /// <summary>
    /// Bottom-right aligned under the button (the menu's right edge on the button's); above it when the space below is short and the space
    /// above is larger; then kept inside the window with <paramref name="margin"/>.
    /// </summary>
    public static MenuSpot Place(double anchorLeft, double anchorTop, double anchorWidth, double anchorHeight, double menuWidth, double menuHeight,
        double windowWidth, double windowHeight, double gap = 4, double margin = 8)
    {
        var below = windowHeight - margin - (anchorTop + anchorHeight + gap);
        var above = anchorTop - gap - margin;
        var up = menuHeight > below && above > below;
        var top = up ? anchorTop - gap - menuHeight : anchorTop + anchorHeight + gap;
        var left = anchorLeft + anchorWidth - menuWidth;
        left = Math.Clamp(left, margin, Math.Max(margin, windowWidth - margin - menuWidth));
        top = Math.Clamp(top, margin, Math.Max(margin, windowHeight - margin - menuHeight));
        return new MenuSpot(left, top, up);
    }
}
