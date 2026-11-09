// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Mods;

namespace Infinity.App;

public sealed partial class ShellViewModel
{
    /// <summary>Why the last launch could not prepare its mods (with the reason), shown on Play until the next attempt.</summary>
    private string? _modLaunchFailure;

    private bool _identityRefused; // B1: the hook ended the game before it read a save (exit 0x46490005)

    /// <summary>A failed mod preparation: logged with its message, shown on Play (never only the status line) and returned for <see cref="Message"/>.</summary>
    private string ModLaunchFailed(string where, Exception error)
    {
        Log("mods: " + where + " failed: " + error.GetType().Name + (error is ModException mod ? " " + mod.Code : "") + ": " + error.Message);
        var text = error is ModException code && code.Code != ModErrors.Failed ? ModErrorText(code) : S["mods.prepare_failed"] + " (" + error.Message + ")";
        _modLaunchFailure = text; Changed(nameof(ModPlayNotice)); Changed(nameof(ShowModPlayNotice));
        return text;
    }
}
