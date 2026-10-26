// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.IO;
using Infinity.Core.Auth;

namespace Infinity.App;

/// <summary>
/// CR-L39 (OD-122 local-first): the server is chosen on the welcome screen, the official one or a custom <c>http(s)://host:port</c>,
/// and saved (<see cref="ServerChoice"/>); the launcher starts again on the new choice, whose state folder is its own. A custom
/// server's pinned keys that change ask the player before anything proceeds.
/// </summary>
public sealed partial class ShellViewModel
{
    private bool _serverEditing;
    private bool _serverCustomSelected;
    private string _serverAddress = string.Empty;
    private string? _serverEditError;
    private TrustChangedException? _trustChange;

    /// <summary>The launcher starts again on the saved choice (MainWindow starts the new process).</summary>
    public event Action? ServerChangeRestartRequested;

    /// <summary>The state root shared by every server's folder (production and custom folders sit under it).</summary>
    private string StateRoot => _config.Production ? Directory.GetParent(_stateDirectory)!.FullName
        : _config.Custom ? Directory.GetParent(Directory.GetParent(_stateDirectory)!.FullName)!.FullName : _stateDirectory;

    public bool UsingCustomServer => _config.Custom;

    public string ServerChoiceText => _config.Custom ? S.Format("server.current_custom", ("server", _config.Server.Authority))
        : _config.Production ? S["server.current_official"]
        : S.Format("server.current_development", ("server", _config.Server.Authority));

    /// <summary>A custom server says whether its connection is encrypted (owner: https shows encrypted, http unencrypted).</summary>
    public bool ShowServerSecurity => _config.Custom;

    public bool ServerEncrypted => _config.Server.Scheme == Uri.UriSchemeHttps;

    public string ServerSecurityText => S[ServerEncrypted ? "server.encrypted" : "server.unencrypted"];

    public bool CanChangeServer => Idle && !Playing && _session is null;

    public bool ServerEditing => _serverEditing;

    /// <summary>The welcome screen's own content, while neither the server editor nor a changed-keys question covers it.</summary>
    public bool ShowWelcomeHome => ShowWelcome && !_serverEditing && _trustChange is null;

    public bool ShowServerEditor => ShowWelcome && _serverEditing && _trustChange is null;

    public bool ServerOfficialSelected
    {
        get => !_serverCustomSelected;
        set { if (value) { _serverCustomSelected = false; _serverEditError = null; Changed(string.Empty); } }
    }

    public bool ServerCustomSelected
    {
        get => _serverCustomSelected;
        set { if (value) { _serverCustomSelected = true; _serverEditError = null; Changed(string.Empty); } }
    }

    public string ServerAddress
    {
        get => _serverAddress;
        set { if (_serverAddress == value) return; _serverAddress = value; _serverEditError = null; Changed(string.Empty); }
    }

    /// <summary>While typing: whether the address would be encrypted, or why it is not a server address.</summary>
    public string? ServerEditText
    {
        get
        {
            if (_serverEditError is not null) return _serverEditError;
            if (!_serverCustomSelected || string.IsNullOrWhiteSpace(_serverAddress)) return null;
            try { return S[CustomServer.ParseOrigin(_serverAddress).Scheme == Uri.UriSchemeHttps ? "server.encrypted" : "server.unencrypted"]; }
            catch (ArgumentException) { return S["server.address_hint"]; }
        }
    }

    public void OpenServerChoice()
    {
        if (!CanChangeServer) return;
        _serverEditing = true;
        _serverCustomSelected = _config.Custom;
        _serverAddress = _config.Custom ? _config.Server.GetLeftPart(UriPartial.Authority) : string.Empty;
        _serverEditError = null;
        Changed(string.Empty);
    }

    public void CancelServerChoice()
    {
        _serverEditing = false;
        _serverEditError = null;
        Changed(string.Empty);
    }

    /// <summary>Saves the choice; true when it differs from the running one and the launcher starts again on it.</summary>
    public bool SaveServerChoice()
    {
        if (!CanChangeServer) return false;
        Uri? custom = null;
        if (_serverCustomSelected)
        {
            try { custom = CustomServer.ParseOrigin(_serverAddress); }
            catch (ArgumentException) { _serverEditError = S["server.invalid_address"]; Changed(string.Empty); return false; }
        }
        try { ServerChoice.Write(StateRoot, custom); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            Log($"server: the choice could not be saved: {e.GetType().Name}");
            _serverEditError = S["server.save_failed"];
            Changed(string.Empty);
            return false;
        }
        _serverEditing = false;
        Log(custom is null ? "server: official server chosen" : "server: custom server chosen");
        var same = custom is null ? !_config.Custom : _config.Custom && _config.Server == custom;
        Changed(string.Empty);
        if (same) return false;
        ServerChangeRestartRequested?.Invoke();
        return true;
    }

    /// <summary>A custom server's keys changed (<see cref="TrustChangedException"/>): nothing proceeds until the player answers.</summary>
    public bool ShowTrustChange => _trustChange is not null;

    public string TrustChangeText => _trustChange is { } change
        ? S.Format("server.keys_changed", ("server", _config.Server.Authority), ("keys", change.Fingerprint))
        : string.Empty;

    private void ObserveTrustChange(TrustChangedException change)
    {
        Log("server: the custom server's keys changed; waiting for the player");
        _trustChange = change;
        Message = null;
        Changed(string.Empty);
    }

    /// <summary>The player trusts the new keys (they asked the server's operator): they are pinned, and sign-in can be tried again.</summary>
    public void AcceptTrustChange()
    {
        if (_trustChange is not { } change || _config.Pins is not { } pins) return;
        try { pins.Pin(change.Origin, change.Realm, change.Published); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException)
        {
            Log($"server: the new keys could not be pinned: {e.GetType().Name}");
            Message = S["server.save_failed"];
            Changed(string.Empty);
            return;
        }
        Log("server: the player accepted the custom server's new keys");
        _trustChange = null;
        Message = S["server.keys_accepted"];
        Changed(string.Empty);
        // a signed-in session still holds the old keys (its /info): start again on the new ones
        if (_session is not null) ServerChangeRestartRequested?.Invoke();
    }

    public void RejectTrustChange()
    {
        _trustChange = null;
        Message = S["server.keys_rejected"];
        Changed(string.Empty);
    }
}
