// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Overlay;

/// <summary>Readable notices for the native overlay's Title\nBody toast format (OD-73).</summary>
public static class OverlayNotices
{
    public static UiFrame.Toast Create(byte level, string body, Strings? text = null)
    {
        level = Math.Min(level, UiLevel.Error);
        var key = level switch
        {
            UiLevel.Error => "overlay.notice_error",
            UiLevel.Warn => "overlay.notice_warning",
            _ => "overlay.notice_info",
        };
        var title = text?[key] ?? "Infinity";
        var icon = level switch { UiLevel.Error => (byte)3, UiLevel.Warn => (byte)2, _ => (byte)0 };
        return new UiFrame.Toast(level, icon, level == UiLevel.Error ? (ushort)150 : (ushort)80,
            title + "\n" + body.Trim());
    }

    public static UiFrame.Toast SaveBlocked(Strings text, string code)
    {
        ArgumentNullException.ThrowIfNull(text);
        var key = code switch
        {
            "SERVICE.UNAVAILABLE" => "saves.seed_unavailable",
            "SAVES.REVISION_CONFLICT" => "saves.conflict",
            "SAVES.LOCAL_BUSY" => "saves.busy",
            _ => "saves.blocked",
        };
        return new UiFrame.Toast(UiLevel.Error, 3, 150,
            text["overlay.save_stopped"] + "\n" + text[key]);
    }
}
