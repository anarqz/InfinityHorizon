// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Text;

namespace Infinity.Core.Process;

/// <summary>How the game process ended (launch.rs <c>classify_exit</c>, ORCHESTRATOR §1.5).</summary>
public abstract record ExitDisposition
{
    private ExitDisposition()
    {
    }

    /// <summary>Exit code the hook uses to ask for a relaunch ('F' 'I' 0x0001; HOOK_MIGRATION.md §3).</summary>
    public const uint ExitRelaunch = 0x4649_0001;

    /// <summary>The exit code of a game the overlay's Quit had to end with <c>TerminateProcess</c>.</summary>
    public const uint QuitForcedExit = 0x4649_0003;

    public sealed record Normal : ExitDisposition;

    public sealed record Relaunch : ExitDisposition;

    /// <summary>NTSTATUS-style failure (0xC0000005 …) or any other non-zero code.</summary>
    public sealed record Crashed(uint Code) : ExitDisposition;

    public static ExitDisposition Classify(uint code) => code switch
    {
        0 or QuitForcedExit => new Normal(),
        ExitRelaunch => new Relaunch(),
        _ => new Crashed(code),
    };
}

/// <summary>
/// What to start: <c>azimuth.exe</c>, its arguments and directory, and the environment changes
/// (launch.rs <c>LaunchPlan</c>). The launcher spawns exactly this, once per play session.
/// </summary>
public sealed record LaunchPlan(string Program, IReadOnlyList<string> Args, string WorkingDirectory, IReadOnlyList<(string Name, string Value)> EnvSet, IReadOnlyList<string> EnvRemove)
{
    /// <summary>
    /// The child's whole environment (ORCHESTRATOR §1.2 step 2): <paramref name="inherited"/> plus
    /// <see cref="EnvSet"/> minus <see cref="EnvRemove"/>. Names compare case-insensitively (on
    /// Windows <c>Path</c> and <c>PATH</c> are one variable), the last value wins, <c>=C:</c>
    /// per-drive entries and empty names are dropped, sorted by upper-cased name as
    /// <c>CreateProcessW</c> wants.
    /// </summary>
    public IReadOnlyList<(string Name, string Value)> Environment(IEnumerable<(string Name, string Value)> inherited)
    {
        ArgumentNullException.ThrowIfNull(inherited);
        var vars = new SortedDictionary<string, (string, string)>(StringComparer.Ordinal);
        foreach (var (name, value) in inherited)
        {
            if (name.Length > 0 && name[0] != '=')
            {
                vars[name.ToUpperInvariant()] = (name, value);
            }
        }
        foreach (var name in EnvRemove)
        {
            vars.Remove(name.ToUpperInvariant());
        }
        foreach (var (name, value) in EnvSet)
        {
            vars[name.ToUpperInvariant()] = (name, value);
        }
        return [.. vars.Values];
    }

    /// <summary>The launcher's own environment as name/value pairs.</summary>
    public static IEnumerable<(string Name, string Value)> CurrentEnvironment()
    {
        foreach (System.Collections.DictionaryEntry entry in System.Environment.GetEnvironmentVariables())
        {
            yield return ((string)entry.Key, (string?)entry.Value ?? string.Empty);
        }
    }

    /// <summary><c>name=value\0…\0\0</c>, the <c>CREATE_UNICODE_ENVIRONMENT</c> block.</summary>
    public static string EnvironmentBlock(IReadOnlyList<(string Name, string Value)> environment)
    {
        ArgumentNullException.ThrowIfNull(environment);
        var block = new StringBuilder();
        foreach (var (name, value) in environment)
        {
            block.Append(name).Append('=').Append(value).Append('\0');
        }
        if (block.Length == 0)
        {
            block.Append('\0');
        }
        return block.Append('\0').ToString();
    }

    /// <summary><c>lpCommandLine</c>: the program and <see cref="Args"/>, each quoted for <c>CommandLineToArgvW</c>.</summary>
    public string WindowsCommandLine()
    {
        var line = new StringBuilder(QuoteWindowsArg(Program));
        foreach (var arg in Args)
        {
            line.Append(' ').Append(QuoteWindowsArg(arg));
        }
        return line.ToString();
    }

    /// <summary>One argument quoted by the rules <c>CommandLineToArgvW</c> and the MSVC CRT parse back (backslashes double only before a quote).</summary>
    public static string QuoteWindowsArg(string arg)
    {
        ArgumentNullException.ThrowIfNull(arg);
        if (arg.Length > 0 && arg.IndexOfAny([' ', '\t', '\n', '\v', '"']) < 0)
        {
            return arg;
        }
        var output = new StringBuilder("\"");
        var backslashes = 0;
        foreach (var c in arg)
        {
            switch (c)
            {
                case '\\':
                    backslashes++;
                    break;
                case '"':
                    output.Append('\\', (backslashes * 2) + 1).Append('"');
                    backslashes = 0;
                    break;
                default:
                    output.Append('\\', backslashes).Append(c);
                    backslashes = 0;
                    break;
            }
        }
        return output.Append('\\', backslashes * 2).Append('"').ToString();
    }
}
