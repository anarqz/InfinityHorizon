// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;

namespace Infinity.Core.Process;

/// <summary>
/// OD-233: ExtraOptions 0.6.0 refuses to run (every feature off, "REFUSED: invalid INI section, option, or value") when
/// <c>FH2_ExtraOptions.ini</c> breaks one rule of its parser, so every ini the launcher writes is checked against those rules first.
/// <see cref="Fh2Problem"/> is <c>fh2/settings.c</c>'s <c>settings_read</c>/<c>parse_ini</c> and <c>fh2/bindings.c</c>'s
/// <c>bindings_parse</c>, line for line (a test holds the setting table equal to the C source). FM5's <c>fm5/fm5.c</c> reads its ini through
/// <c>GetPrivateProfileString</c> and keeps a feature's default for a value that is not exactly <c>0</c> or <c>1</c>
/// (<see cref="Fm5Problem"/>: a value the launcher writes must never fall back that way).
/// </summary>
public static class ExtraOptionsIniRules
{
    /// <summary><c>INI_MAX_SIZE</c>: a larger file is refused.</summary>
    public const int Fh2MaxBytes = 8192;

    /// <summary><c>SettingType</c> of <c>fh2/settings.h</c>.</summary>
    public enum SettingType { Boolean, LiveryCount, TurboInterval, PhotoSamples, Msaa }

    /// <summary><c>setting_definitions</c> then <c>legacy_settings</c> of <c>fh2/settings.c</c> (section, key, type).</summary>
    public static readonly IReadOnlyList<(string Section, string Key, SettingType Type)> Fh2Settings =
    [
        ("Global", "Enabled", SettingType.Boolean),
        ("Content", "UnlockDLC", SettingType.Boolean),
        ("Content", "OfflineVIP", SettingType.Boolean),
        ("Experiments", "LocalDrivatarLiveries", SettingType.Boolean),
        ("Experiments", "MaxDrivatarLiveries", SettingType.LiveryCount),
        ("Frontend", "SkipIntro", SettingType.Boolean),
        ("Frontend", "ShowPlaygroundEvents", SettingType.Boolean),
        ("Frontend", "CarsNextToHome", SettingType.Boolean),
        ("Frontend", "MenuTurboIntervalMs", SettingType.TurboInterval),
        ("Frontend", "AllRimsCategory", SettingType.Boolean),
        ("Experiments", "AllEngineSwaps", SettingType.Boolean),
        ("Experiments", "AllTransmissionSwaps", SettingType.Boolean),
        ("Frontend", "EnableMarketplace", SettingType.Boolean),
        ("Frontend", "EnableSocial", SettingType.Boolean),
        ("Frontend", "EnableClubs", SettingType.Boolean),
        ("Frontend", "EnableRivals", SettingType.Boolean),
        ("Frontend", "GetInCarFromPauseMenu", SettingType.Boolean),
        ("Content", "UnlockStormIslandCars", SettingType.Boolean),
        ("Content", "UnlockTreasureMap", SettingType.Boolean),
        ("Compatibility", "DisableParentalLocks", SettingType.Boolean),
        ("Compatibility", "GenerateMissingSaveCommits", SettingType.Boolean),
        ("PhotoMode", "FreeCamera", SettingType.Boolean),
        ("Files", "EnableMediaOverrides", SettingType.Boolean),
        ("Experiments", "UnlockAeroTuning", SettingType.Boolean),
        ("Autodrive", "Enabled", SettingType.Boolean),
        // legacy_settings: still accepted
        ("Frontend", "AllEngineSwaps", SettingType.Boolean),
        ("Gameplay", "UnlockAeroTuning", SettingType.Boolean),
        ("Livery", "Enable4K", SettingType.Boolean),
        ("Livery", "IntermediateMSAA", SettingType.Msaa),
        ("Livery", "ExtraSupersampling", SettingType.Boolean),
        ("Frontend", "SnapToRoadOnCarChange", SettingType.Boolean),
        ("Frontend", "SkipCarMeetOnStartup", SettingType.Boolean),
        ("Frontend", "SpawnAtFestival", SettingType.Boolean),
        ("Frontend", "SkipIneligibleCarDialogue", SettingType.Boolean),
        ("Graphics", "SmoothSunShadows", SettingType.Boolean),
        ("HUD", "MinimapFollowsCamera", SettingType.Boolean),
        ("PhotoMode", "Samples", SettingType.PhotoSamples),
        ("Autodrive", "Cinematic", SettingType.Boolean),
    ];

    /// <summary><c>known_section</c>'s fixed names (with every section of the setting table).</summary>
    private static readonly string[] ExtraSections = ["Gameplay", "Camera", "Graphics", "Livery", "HUD", "Bindings", "General", "Language", "Mods", "Profile"];

    /// <summary><c>bindings.c</c>'s actions; each has <c>&lt;action&gt;Xbox</c> and <c>&lt;action&gt;Keyboard</c>.</summary>
    public static readonly IReadOnlyList<string> BindingActions =
        ["Autodrive", "CancelAutodrive", "Headlights", "Roof", "SignalLeft", "SignalRight", "Hazards", "PhotoMode",
         "ShowroomPhoto", "ReturnToGarage", "JumpToManufacturer", "ToggleUI", "RimJumpToManufacturer", "CarMeetPhoto"];

    private static readonly (string Name, uint Mask)[] Buttons =
    [
        ("None", 0), ("DPadUp", 1), ("DPadDown", 2), ("DPadLeft", 4), ("DPadRight", 8), ("Menu", 0x10), ("Start", 0x10), ("View", 0x20),
        ("Back", 0x20), ("LeftStick", 0x40), ("LS", 0x40), ("RightStick", 0x80), ("RS", 0x80), ("LB", 0x100), ("RB", 0x200), ("A", 0x1000),
        ("B", 0x2000), ("X", 0x4000), ("Y", 0x8000), ("LT", 0x10000), ("RT", 0x20000),
    ];

    private static readonly string[] Keys =
        ["None", "Escape", "Esc", "Enter", "Space", "Tab", "Backspace", "Insert", "Delete", "Home", "End", "PageUp", "PageDown", "Up", "Down",
         "Left", "Right", "Shift", "Ctrl", "Alt"];

    /// <summary>The game's rule: <see cref="Fh2Problem"/> for FH2, <see cref="Fm5Problem"/> for FM5; null when ExtraOptions accepts the file.</summary>
    public static string? Problem(GameDefinition game, byte[] bytes)
    {
        ArgumentNullException.ThrowIfNull(game);
        ArgumentNullException.ThrowIfNull(bytes);
        return game.Id == "fm5" ? Fm5Problem(bytes) : Fh2Problem(bytes);
    }

    /// <summary>Why FH2's ExtraOptions 0.6.0 refuses these bytes as <c>FH2_ExtraOptions.ini</c>, or null when it reads them.</summary>
    public static string? Fh2Problem(byte[] bytes)
    {
        ArgumentNullException.ThrowIfNull(bytes);
        if (bytes.Length > Fh2MaxBytes) return "larger than 8192 bytes";
        if (bytes.Contains((byte)0)) return "not UTF-8 or ASCII (a NUL byte: UTF-16?)";
        var skip = bytes.Length >= 3 && bytes[0] == 0xef && bytes[1] == 0xbb && bytes[2] == 0xbf ? 3 : 0;
        // The C parser works on bytes and compares ASCII only: Latin-1 keeps every byte as one char.
        var text = Encoding.Latin1.GetString(bytes, skip, bytes.Length - skip);
        var section = "";
        var number = 0;
        foreach (var raw in text.Split('\n'))
        {
            number++;
            var line = raw;
            var comment = line.IndexOfAny([';', '#']);
            if (comment >= 0) line = line[..comment];
            line = Trim(line);
            if (line.Length == 0) continue;
            if (line[0] == '[')
            {
                var closing = line.IndexOf(']');
                if (closing < 0 || Trim(line[(closing + 1)..]).Length > 0) return "line " + number + ": not a section header";
                var name = Trim(line[1..closing]);
                if (name.Length >= 32 || !KnownSection(name)) return "line " + number + ": unknown section [" + name + "]";
                section = name;
                continue;
            }
            var separator = line.IndexOf('=');
            if (separator < 0) return "line " + number + ": no '='";
            var key = Trim(line[..separator]);
            var value = Trim(line[(separator + 1)..]);
            if (Is(section, "Mods") && Is(key, "DatabaseCache") || Is(section, "Frontend") && Is(key, "HideOnlineTabs"))
            {
                if (!Is(value, "0") && !Is(value, "1")) return "line " + number + ": " + key + " must be 0 or 1";
                continue;
            }
            if (Is(section, "Profile") && Is(key, "CustomGamerpic") || Is(section, "Compatibility") && Is(key, "DisableAzimuthSaveChecksums")
                || Is(section, "Language") && (Is(key, "Text") || Is(key, "Audio"))
                || Is(section, "Graphics") && (Is(key, "ForceAnisoFiltering") || Is(key, "MotionBlurSamples")))
                continue; // retired keys: ignored, so an older ini still loads
            if (Is(section, "Camera") && (Is(key, "DriftCamera") || Is(key, "DriftCameraStiffness") || Is(key, "FirstPersonDriftCameraStiffness")))
                continue;
            if (Is(section, "Bindings"))
            {
                if (!Binding(key, value)) return "line " + number + ": invalid binding " + key;
                continue;
            }
            var lookup = Is(section, "General") ? "Global" : section;
            var definition = Fh2Settings.FirstOrDefault(s => Is(s.Section, lookup) && Is(s.Key, key));
            if (definition.Key is null) return "line " + number + ": unknown option " + section + "/" + key;
            if (!ValueValid(definition.Type, value)) return "line " + number + ": invalid value for " + section + "/" + key;
        }
        return null;
    }

    /// <summary>
    /// Why FM5's ExtraOptions 0.6.0 would not take a value of these bytes as written (<c>FM5_ExtraOptions.ini</c>): every
    /// <c>key=value</c> of a section it reads as a switch must be exactly <c>0</c> or <c>1</c> (anything else is logged and the feature's
    /// default used), and the file is ASCII or UTF-8 without NUL bytes. Null when it reads every value.
    /// </summary>
    public static string? Fm5Problem(byte[] bytes)
    {
        ArgumentNullException.ThrowIfNull(bytes);
        if (bytes.Contains((byte)0)) return "not UTF-8 or ASCII (a NUL byte: UTF-16?)";
        var skip = bytes.Length >= 3 && bytes[0] == 0xef && bytes[1] == 0xbb && bytes[2] == 0xbf ? 3 : 0;
        var text = Encoding.Latin1.GetString(bytes, skip, bytes.Length - skip);
        var section = ""; var number = 0;
        foreach (var raw in text.Split('\n'))
        {
            number++;
            var line = raw.Trim(' ', '\t', '\r');
            if (line.Length == 0 || line[0] is ';' or '#') continue;
            if (line[0] == '[') { section = line.Trim('[', ']', ' ', '\t'); continue; }
            var separator = line.IndexOf('=');
            if (separator < 0) continue; // GetPrivateProfileString skips it
            var key = line[..separator].Trim();
            if (!Fm5Switches.Any(s => Is(s.Section, section) && Is(s.Key, key))) continue;
            var value = line[(separator + 1)..].Trim();
            if (value is not ("0" or "1")) return "line " + number + ": " + section + "/" + key + " must be 0 or 1";
        }
        return null;
    }

    /// <summary>The switches <c>fm5/fm5.c</c> reads with <c>option()</c> (0.6.0, before Infinity's patch 0004 removes OfflineVIP).</summary>
    public static readonly IReadOnlyList<(string Section, string Key)> Fm5Switches =
    [
        ("Global", "Enabled"), ("Frontend", "SkipIntro"), ("Frontend", "EnableMarketplace"), ("Compatibility", "DisableParentalLocks"),
        ("Content", "OfflineVIP"), ("Mods", "Enabled"), ("Mods", "DatabaseCache"), ("Files", "EnableMediaOverrides"),
        ("Frontend", "AllRimsCategory"), ("Frontend", "JumpToManufacturer"), ("Frontend", "HideLevelFasterNotification"),
        ("Frontend", "HideDrivatarDesignsButton"), ("Frontend", "E3StartScreen"), ("Camera", "Headlights"),
        ("Compatibility", "GenerateMissingSaveCommits"),
    ];

    private static bool KnownSection(string name) =>
        ExtraSections.Any(s => Is(s, name)) || Fh2Settings.Any(s => Is(s.Section, name));

    /// <summary><c>trim_whitespace</c>: leading spaces and tabs, trailing spaces, tabs and carriage returns.</summary>
    private static string Trim(string text) => text.TrimStart(' ', '\t').TrimEnd(' ', '\t', '\r');

    /// <summary><c>text_equal_ignore_case</c>: ASCII letters only.</summary>
    private static bool Is(string a, string b) => string.Equals(a, b, StringComparison.OrdinalIgnoreCase)
        && a.All(c => c < 128) && b.All(c => c < 128);

    private static bool ValueValid(SettingType type, string text)
    {
        switch (type)
        {
            case SettingType.TurboInterval: return Number(text, 1000) is { } interval && (interval == 0 || interval >= 20);
            case SettingType.LiveryCount: return Number(text, 32) is not null;
            case SettingType.PhotoSamples: return Number(text, 4096) is { } samples && (samples == 0 || samples >= 16);
            case SettingType.Msaa: return text is "2" or "4";
            default: return text is "0" or "1";
        }
    }

    /// <summary><c>parse_value</c>'s digits: at least one, only 0-9, at most <paramref name="maximum"/>.</summary>
    private static uint? Number(string text, uint maximum)
    {
        if (text.Length == 0) return null;
        uint value = 0;
        foreach (var c in text)
        {
            if (c is < '0' or > '9' || value > maximum) return null;
            value = value * 10 + (uint)(c - '0');
        }
        return value > maximum ? null : value;
    }

    /// <summary><c>bindings_parse</c> returning 1: a known <c>&lt;action&gt;Xbox</c> / <c>&lt;action&gt;Keyboard</c> key with a valid value.</summary>
    private static bool Binding(string key, string value)
    {
        foreach (var action in BindingActions)
        {
            if (Is(key, action + "Xbox")) return XboxValue(value);
            if (Is(key, action + "Keyboard")) return KeyboardValue(value);
        }
        return false;
    }

    private static bool XboxValue(string text)
    {
        uint result = 0; var at = 0;
        while (true)
        {
            while (at < text.Length && text[at] is ' ' or '\t') at++;
            var length = 0;
            while (at + length < text.Length && text[at + length] != '+') length++;
            var end = length;
            while (end > 0 && text[at + end - 1] is ' ' or '\t') end--;
            if (end == 0 || end >= 32) return false;
            var token = text.Substring(at, end);
            var button = Buttons.FirstOrDefault(b => Is(b.Name, token));
            var more = at + length < text.Length;
            if (button.Name is null || button.Mask == 0 && (result != 0 || more)) return false;
            result |= button.Mask;
            at += length;
            if (!more) return true;
            at++;
        }
    }

    private static bool KeyboardValue(string text)
    {
        if (Keys.Any(k => Is(k, text))) return true;
        if (text.Length == 1) return char.IsAsciiLetter(text[0]) || char.IsAsciiDigit(text[0]);
        if (text.Length is >= 2 and <= 3 && text[0] is 'F' or 'f')
            return text[1..].All(char.IsAsciiDigit) && int.Parse(text[1..], System.Globalization.CultureInfo.InvariantCulture) is >= 1 and <= 24;
        if (text.Length is >= 3 and <= 4 && text[0] == '0' && text[1] is 'x' or 'X') return text[2..].All(char.IsAsciiHexDigit);
        return false;
    }
}
