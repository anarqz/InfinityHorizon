// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Shell;

/// <summary>
/// OD-118 (G2): how the last run was played, in <c>launch-mode</c> of the state folder. <c>offline</c>: the next start opens offline
/// and never resumes or refreshes a session. <c>online</c>: a kept session resumes. Missing or anything else: the welcome screen.
/// </summary>
public static class LaunchModeStore
{
    public const string Online = "online", Offline = "offline";
    private const string FileName = "launch-mode";

    public static string? Read(string stateDirectory)
    {
        try
        {
            var path = Path.Combine(stateDirectory, FileName);
            if (!File.Exists(path)) return null;
            var value = File.ReadAllText(path).Trim();
            return value is Online or Offline ? value : null;
        }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { return null; }
    }

    public static void Write(string stateDirectory, string mode)
    {
        if (mode is not (Online or Offline)) throw new ArgumentOutOfRangeException(nameof(mode));
        Directory.CreateDirectory(stateDirectory);
        var path = Path.Combine(stateDirectory, FileName);
        var temporary = path + ".tmp";
        File.WriteAllText(temporary, mode);
        File.Move(temporary, path, overwrite: true);
    }
}
