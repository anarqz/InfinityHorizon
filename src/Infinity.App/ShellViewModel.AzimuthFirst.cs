// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>
/// Owner 2026-10-27, "Azimuth is found before sign-in, once per PC, for every server": the start finds Azimuth first (the PC's
/// remembered folder from any server's state folder, then the look-around). When none is found, the first screen is the Azimuth
/// setup, before the welcome screen's SIGN IN, so a new profile can start from the player's Azimuth career. It is local only
/// (OD-118: not a single Infinity request); "Set up later" leads to the welcome screen, and Configure keeps the full setup.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _azimuthFirst;

    /// <summary>The look-around's places (null: <see cref="AzimuthSetup.DefaultRoots"/>); the harness pins it to synthetic folders.</summary>
    internal IEnumerable<string>? AzimuthSearchRoots { get; set; }

    /// <summary>The Azimuth setup in front of the welcome screen (not while a kept session resumes, the server editor or a changed-keys question).</summary>
    public bool ShowAzimuthFirst => _azimuthFirst && ShowWelcome && !_serverEditing && _trustChange is null && !Resuming;

    /// <summary>After the start's search: no Azimuth and still before sign-in, so the setup comes first.</summary>
    private void AskAzimuthFirstWhenMissing()
    {
        if (InShell || _install is not null) return;
        _azimuthFirst = true;
        Log("setup: no Azimuth on this PC: the Azimuth setup is shown before sign-in");
        Changed(string.Empty);
    }

    /// <summary>The folder the player picked on the first screen: kept for the PC (every server) when it holds azimuth.exe.</summary>
    public void ChooseAzimuthFirst(string folder)
    {
        if (!ShowAzimuthFirst || _searching) return;
        bool saved;
        try { saved = AzimuthSetup.SaveChoice(_stateDirectory, folder, StateRoot); }
        catch (Exception e) when (e is System.IO.IOException or UnauthorizedAccessException)
        {
            Log("setup: Azimuth choice not kept: " + e.GetType().Name);
            saved = false;
        }
        if (!saved) { Message = S["setup.not_azimuth_folder"]; return; }
        Install = AzimuthInstall.Detect(folder);
        Log($"setup: Azimuth {_install?.Version ?? "?"} at {_install?.Root} chosen before sign-in ({SetupStatus})");
        FinishAzimuthFirst();
    }

    /// <summary>"Check again": the same search as the start (the remembered folder of any server, then the look-around).</summary>
    public async Task RecheckAzimuthFirstAsync()
    {
        if (!ShowAzimuthFirst || _searching) return;
        await FindAzimuthAsync(null, openSetupWhenNeeded: false);
        if (_install is not null) FinishAzimuthFirst();
        else Message = S["setup.not_found"];
    }

    /// <summary>"Set up later": the welcome screen; Configure still has the setup after sign-in or Play offline.</summary>
    public void SetUpAzimuthLater()
    {
        if (!_azimuthFirst) return;
        _azimuthFirst = false;
        Message = null;
        Log("setup: Azimuth setup left for later (welcome screen)");
        Changed(string.Empty);
    }

    private void FinishAzimuthFirst()
    {
        _azimuthFirst = false;
        Message = S.Format("setup.first_found", ("path", _install?.Root ?? string.Empty));
        Changed(string.Empty);
    }
}
