// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Runtime.Versioning;
using System.Threading.Channels;
using Infinity.Core.Process;
using Microsoft.Win32.SafeHandles;

namespace Infinity.Core.Windows;

/// <summary>How the child is supervised: through its job, or by process handle only (ORCHESTRATOR §1.2 step 4).</summary>
public abstract record Supervise
{
    private Supervise()
    {
    }

    public sealed record Job : Supervise;

    /// <summary>The job could not be made or joined (an old Wine, a launcher inside a job that forbids nesting).</summary>
    public sealed record HandleOnly(string Reason) : Supervise;
}

public enum JobMessage : uint
{
    ActiveProcessZero = 4,
    NewProcess = 6,
    ExitProcess = 7,
    AbnormalExitProcess = 8,
}

/// <summary>One message from the job's completion port (logged with pids only).</summary>
public sealed record JobEvent(JobMessage Message, uint Pid);

/// <summary>
/// <c>azimuth.exe</c> as the launcher starts and supervises it (ORCHESTRATOR §1.2-§1.4; spawn.rs,
/// job.rs). <see cref="Spawn"/> creates an anonymous job with a completion port and no
/// kill-on-close, starts the child suspended with no inherited handles, puts it in the job before
/// its first instruction, and resumes it. The game is over when its process handle is signalled.
/// Nothing here kills the game except <see cref="QuitAsync"/>, the player's confirmed Quit, and a
/// child that never started because <c>ResumeThread</c> failed.
/// </summary>
[SupportedOSPlatform("windows")]
public sealed class GameProcess : Supervisor.IGameChild
{
    private readonly SafeProcessHandle _process;
    private readonly IntPtr _job;
    private readonly IntPtr _port;
    private readonly ManualResetEvent _exited;
    private readonly Channel<JobEvent> _jobEvents = Channel.CreateUnbounded<JobEvent>(new UnboundedChannelOptions { SingleReader = true });
    private int _disposed;

    private GameProcess(SafeProcessHandle process, uint pid, ulong? started, IntPtr job, IntPtr port, Supervise supervise)
    {
        _process = process;
        Pid = pid;
        Started = started;
        _job = job;
        _port = port;
        Supervise = supervise;
        _exited = new ManualResetEvent(false) { SafeWaitHandle = new SafeWaitHandle(process.DangerousGetHandle(), ownsHandle: false) };
        if (port != IntPtr.Zero)
        {
            new Thread(PumpJob) { IsBackground = true, Name = "fi-job-pump" }.Start();
        }
        else
        {
            _jobEvents.Writer.TryComplete();
        }
    }

    public uint Pid { get; }

    /// <summary><c>GetProcessTimes</c> creation time (session.json <c>child_started</c>): a reused pid never counts as the game.</summary>
    public ulong? Started { get; }

    public DateTimeOffset? ExitTimeUtc => _exited.WaitOne(0) && Native.GetProcessTimes(_process, out _, out var ended, out _, out _)
        && ended != 0 && ended <= long.MaxValue ? DateTimeOffset.FromFileTime((long)ended) : null;

    public Supervise Supervise { get; }

    /// <summary>Signalled when the game process exits; for <see cref="PipeServer.AcceptAsync"/>.</summary>
    public WaitHandle Exited => _exited;

    /// <summary>The job's messages; completes when the job is closed (none with <see cref="Supervise.HandleOnly"/>).</summary>
    public ChannelReader<JobEvent> JobEvents => _jobEvents.Reader;

    public static GameProcess Spawn(LaunchPlan plan) => Spawn(plan, LaunchPlan.CurrentEnvironment());

    public static GameProcess Spawn(LaunchPlan plan, IEnumerable<(string Name, string Value)> inherited)
    {
        ArgumentNullException.ThrowIfNull(plan);
        // 1. The job first, so a failure to make one is known before the process exists.
        var (job, port, jobError) = CreateJob();
        // 2-3. Environment, command line, suspended process, no handle inheritance.
        var environment = Marshal.StringToHGlobalUni(LaunchPlan.EnvironmentBlock(plan.Environment(inherited)));
        Native.ProcessInformation info;
        try
        {
            var startup = new Native.StartupInfo { Cb = Marshal.SizeOf<Native.StartupInfo>() };
            if (!Native.CreateProcessW(plan.Program, (plan.WindowsCommandLine() + "\0").ToCharArray(), IntPtr.Zero, IntPtr.Zero, false,
                    Native.CreateSuspended | Native.CreateUnicodeEnvironment, environment, plan.WorkingDirectory, ref startup, out info))
            {
                var error = Marshal.GetLastPInvokeError();
                CloseJob(job, port);
                throw new Win32Exception(error, $"CreateProcessW {plan.Program}");
            }
        }
        finally
        {
            Marshal.FreeHGlobal(environment);
        }
        var process = new SafeProcessHandle(info.Process, ownsHandle: true);
        // 4. Into the job before the first instruction; never abort the launch for it.
        Supervise supervise;
        if (job == IntPtr.Zero)
        {
            supervise = new Supervise.HandleOnly($"CreateJobObjectW/CreateIoCompletionPort: {jobError}");
        }
        else if (!Native.AssignProcessToJobObject(job, process))
        {
            var error = Marshal.GetLastPInvokeError();
            var why = error == Native.ErrorAccessDenied
                ? "AssignProcessToJobObject: access denied (the launcher's own job forbids nesting)"
                : "AssignProcessToJobObject failed";
            supervise = new Supervise.HandleOnly($"{why}: {new Win32Exception(error).Message}");
            CloseJob(job, port);
            (job, port) = (IntPtr.Zero, IntPtr.Zero);
        }
        else
        {
            supervise = new Supervise.Job();
        }
        // 5. Run it. A child whose ResumeThread failed never ran an instruction: it is not the
        // game yet (§1.2 step 5), and left suspended it would hold a pid and the game's files.
        var resumed = Native.ResumeThread(info.Thread);
        var resumeError = Marshal.GetLastPInvokeError();
        Native.CloseHandle(info.Thread);
        if (resumed == uint.MaxValue)
        {
            Native.TerminateProcess(process, 1);
            process.Dispose();
            CloseJob(job, port);
            throw new Win32Exception(resumeError, "ResumeThread");
        }
        return new GameProcess(process, info.ProcessId, CreationTime(process), job, port, supervise);
    }

    /// <summary>The exit code once the process has exited, otherwise null.</summary>
    public uint? ExitCode => _exited.WaitOne(0) && Native.GetExitCodeProcess(_process, out var code) ? code : null;

    public async Task<uint> WaitForExitAsync(CancellationToken cancellation = default)
    {
        var done = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var registration = ThreadPool.RegisterWaitForSingleObject(_exited, (_, _) => done.TrySetResult(), null, Timeout.Infinite, executeOnlyOnce: true);
        try
        {
            await done.Task.WaitAsync(cancellation).ConfigureAwait(false);
        }
        finally
        {
            registration.Unregister(null);
        }
        return ExitCode ?? throw new Win32Exception(Marshal.GetLastPInvokeError(), "GetExitCodeProcess");
    }

    /// <summary>Posts <c>WM_CLOSE</c> to the game's visible top-level windows; true when at least one was found.</summary>
    public bool RequestClose()
    {
        var posted = 0;
        Native.EnumWindows((hwnd, _) =>
        {
            if (Native.GetWindowThreadProcessId(hwnd, out var owner) != 0 && owner == Pid && Native.IsWindowVisible(hwnd) && Native.PostMessageW(hwnd, Native.WmClose, IntPtr.Zero, IntPtr.Zero))
            {
                posted++;
            }
            return true;
        }, IntPtr.Zero);
        return posted > 0;
    }

    /// <summary>
    /// The player's confirmed Quit (UiEvent 7): <c>WM_CLOSE</c> to the game's windows, then
    /// <c>TerminateProcess</c> with <see cref="ExitDisposition.QuitForcedExit"/> when it is still
    /// running after <paramref name="grace"/> (20 s in the supervisor). Returns true when it had to force.
    /// </summary>
    public async Task<bool> QuitAsync(TimeSpan grace, CancellationToken cancellation = default)
    {
        if (_exited.WaitOne(0))
        {
            return false;
        }
        RequestClose();
        try
        {
            await WaitForExitAsync(cancellation).WaitAsync(grace, cancellation).ConfigureAwait(false);
            return false;
        }
        catch (TimeoutException)
        {
            return Native.TerminateProcess(_process, ExitDisposition.QuitForcedExit);
        }
    }

    /// <summary>The creation stamp of <paramref name="pid"/>, or null when it is not running (session.json recovery, §2.4).</summary>
    public static ulong? ProcessStarted(uint pid)
    {
        using var handle = Native.OpenProcess(Native.ProcessQueryLimitedInformation, false, pid);
        return handle.IsInvalid ? null : CreationTime(handle);
    }

    private static ulong? CreationTime(SafeProcessHandle process) =>
        Native.GetProcessTimes(process, out var created, out _, out _, out _) ? created : null;

    private static (IntPtr Job, IntPtr Port, string? Error) CreateJob()
    {
        var job = Native.CreateJobObjectW(IntPtr.Zero, null);
        if (job == IntPtr.Zero)
        {
            return (IntPtr.Zero, IntPtr.Zero, new Win32Exception(Marshal.GetLastPInvokeError()).Message);
        }
        var port = Native.CreateIoCompletionPort(new IntPtr(-1), IntPtr.Zero, UIntPtr.Zero, 1);
        if (port == IntPtr.Zero)
        {
            var error = new Win32Exception(Marshal.GetLastPInvokeError()).Message;
            Native.CloseHandle(job);
            return (IntPtr.Zero, IntPtr.Zero, error);
        }
        // No JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE and no breakaway: the default limits are none.
        var association = new Native.JobAssociateCompletionPort { CompletionKey = IntPtr.Zero, CompletionPort = port };
        if (!Native.SetInformationJobObject(job, Native.JobObjectAssociateCompletionPortInformation, ref association, Marshal.SizeOf<Native.JobAssociateCompletionPort>()))
        {
            var error = new Win32Exception(Marshal.GetLastPInvokeError()).Message;
            CloseJob(job, port);
            return (IntPtr.Zero, IntPtr.Zero, error);
        }
        return (job, port, null);
    }

    private static void CloseJob(IntPtr job, IntPtr port)
    {
        if (job != IntPtr.Zero)
        {
            Native.CloseHandle(job);
        }
        if (port != IntPtr.Zero)
        {
            Native.CloseHandle(port);
        }
    }

    private void PumpJob()
    {
        // Ends when the port is closed (Dispose): GetQueuedCompletionStatus then fails.
        while (Native.GetQueuedCompletionStatus(_port, out var message, out _, out var pid, Timeout.Infinite))
        {
            if (Enum.IsDefined((JobMessage)message))
            {
                _jobEvents.Writer.TryWrite(new JobEvent((JobMessage)message, (uint)pid.ToInt64()));
            }
        }
        _jobEvents.Writer.TryComplete();
    }

    /// <summary>Closes the handles. The game keeps running: the job has no kill-on-close.</summary>
    public void Dispose()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0)
        {
            return;
        }
        _exited.Dispose();
        CloseJob(_job, _port);
        _process.Dispose();
    }

    private static class Native
    {
        public const uint CreateSuspended = 0x00000004;
        public const uint CreateUnicodeEnvironment = 0x00000400;
        public const int ErrorAccessDenied = 5;
        public const int JobObjectAssociateCompletionPortInformation = 7;
        public const uint ProcessQueryLimitedInformation = 0x1000;
        public const uint WmClose = 0x0010;

        [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
        public struct StartupInfo
        {
            public int Cb;
            public IntPtr Reserved;
            public IntPtr Desktop;
            public IntPtr Title;
            public int X, Y, XSize, YSize, XCountChars, YCountChars, FillAttribute, Flags;
            public short ShowWindow, Reserved2;
            public IntPtr Reserved3, StdInput, StdOutput, StdError;
        }

        [StructLayout(LayoutKind.Sequential)]
        public struct ProcessInformation
        {
            public IntPtr Process;
            public IntPtr Thread;
            public uint ProcessId;
            public uint ThreadId;
        }

        [StructLayout(LayoutKind.Sequential)]
        public struct JobAssociateCompletionPort
        {
            public IntPtr CompletionKey;
            public IntPtr CompletionPort;
        }

        public delegate bool EnumWindowsProc(IntPtr hwnd, IntPtr data);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool CreateProcessW(string application, [MarshalAs(UnmanagedType.LPArray)] char[] commandLine, IntPtr processAttributes, IntPtr threadAttributes,
            [MarshalAs(UnmanagedType.Bool)] bool inheritHandles, uint flags, IntPtr environment, string currentDirectory, ref StartupInfo startup, out ProcessInformation info);

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        public static extern IntPtr CreateJobObjectW(IntPtr attributes, string? name);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        public static extern IntPtr CreateIoCompletionPort(IntPtr file, IntPtr existingPort, UIntPtr key, uint threads);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool SetInformationJobObject(IntPtr job, int infoClass, ref JobAssociateCompletionPort info, int length);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool AssignProcessToJobObject(IntPtr job, SafeProcessHandle process);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool GetQueuedCompletionStatus(IntPtr port, out uint bytes, out UIntPtr key, out IntPtr overlapped, int milliseconds);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        public static extern uint ResumeThread(IntPtr thread);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool CloseHandle(IntPtr handle);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool TerminateProcess(SafeProcessHandle process, uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool GetExitCodeProcess(SafeProcessHandle process, out uint exitCode);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool GetProcessTimes(SafeProcessHandle process, out ulong creation, out ulong exit, out ulong kernel, out ulong user);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        public static extern SafeProcessHandle OpenProcess(uint access, [MarshalAs(UnmanagedType.Bool)] bool inherit, uint pid);

        [DllImport("user32.dll", ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool EnumWindows(EnumWindowsProc callback, IntPtr data);

        [DllImport("user32.dll", ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);

        [DllImport("user32.dll", ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool IsWindowVisible(IntPtr hwnd);

        [DllImport("user32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool PostMessageW(IntPtr hwnd, uint message, IntPtr wParam, IntPtr lParam);
    }
}
