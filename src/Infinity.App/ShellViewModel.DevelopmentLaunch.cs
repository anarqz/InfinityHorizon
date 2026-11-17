// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core;
using Infinity.Core.Auth;
using Infinity.Core.Process;

namespace Infinity.App;

/// <summary>
/// DEVLAUNCH (batch 1118): a development build plays only the Azimuth folder its command line names with <c>--azimuth</c>. It never
/// falls back to a saved, found or picked folder (that folder would get the development hook and the mods' files); without
/// <c>--azimuth</c> it uses none and says how to start it. Its command-line server stays a development server in the picker.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _azimuthPinned;
    private string? _pinnedAzimuth;

    /// <summary>A development build's start (MainWindow, after the harness): the one folder it may use, or none.</summary>
    internal void PinDevelopmentAzimuth(string? folder)
    {
        _azimuthPinned = true;
        _pinnedAzimuth = folder;
        Log(folder is null ? "setup: development build without --azimuth: no Azimuth folder is used" : $"setup: development build: only {folder} is used");
    }

    /// <summary>The refusal said when a development build would use another folder (or has none).</summary>
    private string DevelopmentAzimuthRefusal => _pinnedAzimuth is null ? S["setup.dev_azimuth_required"]
        : S.Format("setup.dev_azimuth_other", ("path", _pinnedAzimuth));

    /// <summary>The install to select: unchanged, except in a development build, where only the <c>--azimuth</c> folder passes.</summary>
    private AzimuthInstall? AllowedInstall(AzimuthInstall? value)
    {
        if (!_azimuthPinned || value is null || DevelopmentLaunch.AllowsAzimuth(_pinnedAzimuth, value.Root)) return value;
        Log($"setup: development build: {value.Root} refused (not the --azimuth folder)");
        Message = DevelopmentAzimuthRefusal;
        return null;
    }

    /// <summary>A folder picked by hand in a development build that is not its <c>--azimuth</c> one: refused and said.</summary>
    private bool RefusesPickedAzimuth(string folder)
    {
        if (!_azimuthPinned || DevelopmentLaunch.AllowsAzimuth(_pinnedAzimuth, folder)) return false;
        Log("setup: development build: a picked Azimuth folder was refused (not the --azimuth folder)");
        Message = DevelopmentAzimuthRefusal;
        return true;
    }

    /// <summary>
    /// A development build's command-line server, which the picker's first row stands for: the running one, or on a custom server the
    /// one the build was started with (kept in its state root, <see cref="DevelopmentLaunch.ReadCommandLineServer"/>); never the default.
    /// </summary>
    private Uri? DevelopmentServer => BuildProfile.ProductionOnly || _config.Production ? null
        : !_config.Custom ? _config.Server
        : (_keptDevelopmentServer ??= new(DevelopmentLaunch.ReadCommandLineServer(StateRoot)?.Server)).Server;

    private sealed record KeptServer(Uri? Server);
    private KeptServer? _keptDevelopmentServer;

    /// <summary>The sign-in form names the server it signs in to on a custom server and in a development build.</summary>
    public bool ShowSignInServer => _config.Custom || DevelopmentServer is not null;
}
