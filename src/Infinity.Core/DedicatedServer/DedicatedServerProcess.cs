// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.ComponentModel;
using System.Diagnostics;
using System.Globalization;
using System.Runtime.InteropServices;
using System.Runtime.Versioning;
using System.Security.Cryptography;
using System.Threading.Channels;
using Infinity.Core.Matchmaking;
using Infinity.Core.Windows;
using Microsoft.Win32.SafeHandles;

namespace Infinity.Core.DedicatedServer;

/// <summary>What the launcher hands one private <c>dedicated-server</c> child (DEDICATED_SERVER.md §2).</summary>
public sealed record DsLaunchOptions
{
    /// <summary>The fixed path next to <c>infinity.exe</c> (<see cref="DedicatedServerProcess.ResolveExecutable"/>); never searched on PATH.</summary>
    public required string Executable { get; init; }

    /// <summary>The launcher's version, sent as <c>hello.controller.version</c>.</summary>
    public required string ControllerVersion { get; init; }

    /// <summary>The runtime descriptor's version of the exe; a different <c>hello.build.version</c> refuses hosting (§10). Null: not checked.</summary>
    public string? ExpectedVersion { get; init; }

    /// <summary><c>--game-db</c>: the player's <c>gamedbRC.slt</c> (<c>RelayNetwork.FindGameDatabase</c>), opened read-only by the engine.</summary>
    public string? GameDatabase { get; init; }

    /// <summary><c>--grant-keys</c>: the relay-grant public ring (<c>kid:base64,…</c>); replaceable with <c>keys.set</c>.</summary>
    public string? GrantKeys { get; init; }

    public int? DrainSeconds { get; init; }

    /// <summary><c>error</c>, <c>warn</c>, <c>info</c> or <c>debug</c>.</summary>
    public string? LogLevel { get; init; }

    /// <summary>The child's connect bound plus slack (the child retries for 5 s, then exits 3).</summary>
    public TimeSpan ConnectTimeout { get; init; } = TimeSpan.FromSeconds(8);

    /// <summary>The launcher log (scrubbed lines only). A throwing sink is ignored.</summary>
    public Action<string>? Log { get; init; }

    public DsClientOptions? Client { get; init; }

    /// <summary>TEST ONLY: extra arguments after the contract's (the fake engine's mode).</summary>
    internal IReadOnlyList<string> TestOnlyExtraArguments { get; init; } = [];

    /// <summary>TEST ONLY: extra variables in the child's own environment block.</summary>
    internal IReadOnlyDictionary<string, string> TestOnlyExtraEnvironment { get; init; } = new Dictionary<string, string>();

    /// <summary>TEST ONLY: a different secret in <c>hello</c> than in the child's environment.</summary>
    internal string? TestOnlyHelloSecret { get; init; }

    /// <summary>TEST ONLY: <c>hello.proto</c> in the older <c>[1, 1]</c> form (a smoke against an engine build that has not caught up).</summary>
    internal bool TestOnlyLegacyHelloProtoRange { get; init; }

    /// <summary>TEST ONLY: this pipe name instead of a fresh random one (the squatted-name check).</summary>
    internal string? TestOnlyPipeName { get; init; }

    /// <summary>TEST ONLY: no job object (the handle-only supervision of an old Wine or a nesting-forbidding job).</summary>
    internal bool TestOnlyNoJob { get; init; }
}

/// <summary>The child could not be brought up; <see cref="Exit"/> says whether hosting is unavailable.</summary>
public sealed class DedicatedServerStartException(DsExit exit) : Exception($"dedicated-server did not start: {exit.Kind} ({exit.Detail})")
{
    public DsExit Exit { get; } = exit;
}

/// <summary>How the child is held: in a kill-on-close job, or by its handle only (an old Wine, a launcher job that forbids nesting).</summary>
public enum DsSupervision
{
    Job,
    HandleOnly,
}

/// <summary>
/// One private <c>dedicated-server</c> child (DEDICATED_SERVER.md §2-§3.1; CR-L50): the launcher creates the control
/// pipe first (<see cref="PipeServer"/>: user-SID DACL, first instance, reject remote, one byte-mode instance) as
/// <c>\\.\pipe\fi-ds-&lt;32 hex&gt;</c>, starts the fixed-path exe with the contract's arguments and the per-launch
/// secret in the child's own environment block (never argv, never the launcher's environment, no inherited
/// <c>FI_DS_*</c>), puts it in a <c>JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE</c> job right after it starts, accepts only
/// a client whose PID is the child's, sends <c>hello</c>, and checks the build version. stdout and stderr are read
/// on threads of their own, scrubbed (<see cref="DsLogScrubber"/>), into a bounded, dropping log queue; a stdout line
/// that is not JSON or over 1 MiB, a protocol violation on the pipe, or a stalled write kills the child. Any exit or EOF
/// ends every world it served: one <see cref="DsGone"/> event closes <see cref="Events"/>, and the process never
/// restarts itself or re-opens a world. No exception leaves any of its background threads or tasks.
/// </summary>
[SupportedOSPlatform("windows")]
public sealed class DedicatedServerProcess : IAsyncDisposable
{
    public const string ExecutableName = "dedicated-server.exe";

    /// <summary>The window the child has, after EOF on its pipe, to exit before it is killed (§2.2: it exits within 2 s).</summary>
    public static readonly TimeSpan EofGrace = TimeSpan.FromSeconds(3);

    /// <summary>The bound of every wait in <see cref="DisposeAsync"/>.</summary>
    public static readonly TimeSpan DisposeBound = TimeSpan.FromSeconds(5);

    private const int LogQueueSize = 512;
    private const int MaxStderrLines = 64;

    /// <summary>The most stderr the launcher reads per run (§2.3: the engine writes at most 64 KiB); the rest is drained, never forwarded.</summary>
    private const int MaxStderrBytes = 64 * 1024;

    private readonly System.Diagnostics.Process _process;
    private readonly PipeServer _pipe;
    private readonly IntPtr _job;
    private readonly DsLaunchOptions _options;
    private readonly DsLogScrubber _scrubber;
    private readonly ManualResetEvent _exitedHandle;
    private readonly TaskCompletionSource<DsExit> _exited = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly Channel<string> _log = Channel.CreateBounded<string>(new BoundedChannelOptions(LogQueueSize) { SingleReader = true, FullMode = BoundedChannelFullMode.Wait });
    private DsExit? _launcherReason;
    private int _jobClosed;
    private long _logDropped;
    private int _disposed;
    private int _watching;

    private DedicatedServerProcess(System.Diagnostics.Process process, PipeServer pipe, IntPtr job, DsLaunchOptions options, IReadOnlyList<string> arguments, string secret)
    {
        _process = process;
        _pipe = pipe;
        _job = job;
        _options = options;
        _scrubber = new DsLogScrubber(secret, pipe.Name);
        Arguments = arguments;
        Pid = (uint)process.Id;
        PipeName = pipe.Name;
        Supervision = job == IntPtr.Zero ? DsSupervision.HandleOnly : DsSupervision.Job;
        _exitedHandle = new ManualResetEvent(false) { SafeWaitHandle = new SafeWaitHandle(process.Handle, ownsHandle: false) };
        Client = new DedicatedServerClient(pipe.Stream, (options.Client ?? new DsClientOptions()) with
        {
            CompleteEventsOnClose = false,
            Log = options.Client?.Log ?? options.Log,
        });
        Client.OnViolation = field => Kill(field == "stalled"
            ? new DsExit(DsExitKind.Stalled, null, "control write stalled")
            : new DsExit(DsExitKind.ProtocolViolation, null, "control " + field));
        _ = Task.Run(PumpLogAsync);
    }

    public uint Pid { get; }

    public string PipeName { get; }

    public DsSupervision Supervision { get; }

    /// <summary>The arguments the child was started with (the secret is never among them).</summary>
    public IReadOnlyList<string> Arguments { get; }

    public DedicatedServerClient Client { get; }

    /// <summary>The engine's <c>hello</c> answer.</summary>
    public DsHelloResult Hello { get; private set; } = null!;

    /// <summary>Every event of the child, then one <see cref="DsGone"/>, then the end.</summary>
    public ChannelReader<DsEvent> Events => Client.Events;

    /// <summary>Completes once the child has exited and <see cref="DsGone"/> was published.</summary>
    public Task<DsExit> Exited => _exited.Task;

    /// <summary>stdout/stderr lines the bounded log queue had to drop.</summary>
    public long LogDropped => Interlocked.Read(ref _logDropped);

    /// <summary>The fixed path of the exe next to the launcher's own (<c>infinity.exe</c>'s directory by default).</summary>
    public static string ResolveExecutable(string? launcherDirectory = null)
    {
        var directory = launcherDirectory ?? Path.GetDirectoryName(Environment.ProcessPath) ?? AppContext.BaseDirectory;
        return Path.GetFullPath(Path.Combine(directory, ExecutableName));
    }

    /// <summary><c>\\.\pipe\fi-ds-&lt;32 lowercase hex&gt;</c>, 16 random bytes.</summary>
    public static string NewPipeName() => @"\\.\pipe\fi-ds-" + Convert.ToHexStringLower(RandomNumberGenerator.GetBytes(16));

    /// <summary>64 lowercase hex, 32 random bytes per launch.</summary>
    public static string NewSecret() => Convert.ToHexStringLower(RandomNumberGenerator.GetBytes(32));

    /// <summary>The contract's arguments (§2): <c>--profile private --control pipe:&lt;name&gt;</c> and the options given.</summary>
    public static IReadOnlyList<string> BuildArguments(DsLaunchOptions options, string pipeName)
    {
        ArgumentNullException.ThrowIfNull(options);
        var args = new List<string> { "--profile", "private", "--control", "pipe:" + pipeName };
        if (options.GameDatabase is { Length: > 0 } db)
        {
            args.AddRange(["--game-db", Path.GetFullPath(db)]);
        }
        if (options.GrantKeys is { Length: > 0 } keys)
        {
            args.AddRange(["--grant-keys", keys]);
        }
        if (options.DrainSeconds is { } drain)
        {
            args.AddRange(["--drain-seconds", drain.ToString(CultureInfo.InvariantCulture)]);
        }
        if (options.LogLevel is { Length: > 0 } level)
        {
            args.AddRange(["--log-level", level]);
        }
        args.AddRange(options.TestOnlyExtraArguments);
        return args;
    }

    /// <summary>The child's environment: the launcher's, minus every inherited <c>FI_DS_*</c>, plus the secret.</summary>
    internal static void PrepareEnvironment(IDictionary<string, string?> environment, string secret, IReadOnlyDictionary<string, string> extra)
    {
        foreach (var name in environment.Keys.Where(k => k.StartsWith(DsProto.EnvPrefix, StringComparison.OrdinalIgnoreCase)).ToArray())
        {
            environment.Remove(name);
        }
        environment[DsProto.SecretEnv] = secret;
        foreach (var (name, value) in extra)
        {
            environment[name] = value;
        }
    }

    /// <summary>Creates the pipe, starts the child, accepts it, and completes <c>hello</c>. Throws <see cref="DedicatedServerStartException"/> only.</summary>
    public static async Task<DedicatedServerProcess> StartAsync(DsLaunchOptions options, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(options);
        if (!Path.IsPathFullyQualified(options.Executable) || !File.Exists(options.Executable))
        {
            throw new DedicatedServerStartException(new DsExit(DsExitKind.ConfigInvalid, null, "dedicated-server.exe is missing"));
        }
        var name = options.TestOnlyPipeName ?? NewPipeName();
        PipeServer pipe;
        try
        {
            // first instance: a name squatted before us fails here, and hosting is aborted (§3.1)
            pipe = PipeServer.Create(name);
        }
        catch (Exception e) when (e is Win32Exception or IOException or UnauthorizedAccessException or InvalidOperationException)
        {
            throw new DedicatedServerStartException(new DsExit(DsExitKind.ControlUnavailable, null, "pipe: " + (e is Win32Exception w ? w.NativeErrorCode.ToString(CultureInfo.InvariantCulture) : e.GetType().Name)));
        }
        var secret = NewSecret();
        var arguments = BuildArguments(options, name);
        var start = new ProcessStartInfo(options.Executable)
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            WorkingDirectory = Path.GetDirectoryName(options.Executable)!,
        };
        foreach (var argument in arguments)
        {
            start.ArgumentList.Add(argument);
        }
        PrepareEnvironment(start.Environment, secret, options.TestOnlyExtraEnvironment);
        System.Diagnostics.Process process;
        try
        {
            process = System.Diagnostics.Process.Start(start) ?? throw new InvalidOperationException("no process");
        }
        catch (Exception e) when (e is Win32Exception or InvalidOperationException or IOException)
        {
            pipe.Dispose();
            throw new DedicatedServerStartException(new DsExit(DsExitKind.ConfigInvalid, null, "start: " + e.GetType().Name));
        }
        // right after start: a crashed launcher never leaves an orphan (§2.2)
        string? jobError = "test only: no job";
        var job = options.TestOnlyNoJob ? IntPtr.Zero : AssignKillOnCloseJob(process, out jobError);
        var child = new DedicatedServerProcess(process, pipe, job, options, arguments, secret);
        try
        {
            process.StandardInput.Close();
            if (job == IntPtr.Zero)
            {
                child.LogLine("ds job: handle only (" + jobError + ")");
            }
            child.StartReaders();
            await child.ConnectAsync(options, options.TestOnlyHelloSecret ?? secret, cancellation).ConfigureAwait(false);
        }
        catch (Exception e)
        {
            var exit = e is DedicatedServerStartException s ? s.Exit : child.Stop(new DsExit(DsExitKind.ControlUnavailable, null, "start: " + e.GetType().Name));
            await child.DisposeAsync().ConfigureAwait(false);
            if (e is OperationCanceledException && cancellation.IsCancellationRequested)
            {
                throw;
            }
            throw e as DedicatedServerStartException ?? new DedicatedServerStartException(exit);
        }
        child.Watch();
        return child;
    }

    private async Task ConnectAsync(DsLaunchOptions options, string secret, CancellationToken cancellation)
    {
        var accepted = await _pipe.AcceptAsync(Pid, _exitedHandle, options.ConnectTimeout, cancellation).ConfigureAwait(false);
        if (accepted != AcceptResult.Connected)
        {
            throw new DedicatedServerStartException(accepted == AcceptResult.ChildExited
                ? await ExitAsync().ConfigureAwait(false)
                : Stop(new DsExit(DsExitKind.ConnectTimedOut, null, "the child did not connect")));
        }
        Client.Start();
        DsResult<DsHelloResult> hello;
        try
        {
            hello = await Client.HelloAsync(secret, options.ControllerVersion, options.TestOnlyLegacyHelloProtoRange, cancellation).ConfigureAwait(false);
        }
        catch (Exception e) when (e is not OperationCanceledException || !cancellation.IsCancellationRequested)
        {
            // unauthorized / proto_unsupported close the channel and exit 4 / 5; anything else is "no control"
            var why = e is TimeoutException ? DsExitKind.ConnectTimedOut : DsExitKind.ControlUnavailable;
            throw new DedicatedServerStartException(await ExitWithinAsync(EofGrace, new DsExit(why, null, "hello: " + e.GetType().Name)).ConfigureAwait(false));
        }
        if (!hello.Ok)
        {
            var fallback = hello.Error?.Code switch
            {
                DsErrorCodes.Unauthorized => new DsExit(DsExitKind.Unauthorized, null, "hello unauthorized"),
                DsErrorCodes.ProtoUnsupported => new DsExit(DsExitKind.ProtoUnsupported, null, "hello proto_unsupported"),
                var code => new DsExit(DsExitKind.ControlUnavailable, null, "hello " + (code ?? "superseded")),
            };
            throw new DedicatedServerStartException(await ExitWithinAsync(EofGrace, fallback).ConfigureAwait(false));
        }
        if (hello.Value is not { Build.Version: not null } value)
        {
            throw new DedicatedServerStartException(Stop(new DsExit(DsExitKind.ControlUnavailable, null, "hello without a build")));
        }
        Hello = value;
        if (options.ExpectedVersion is { } expected && !string.Equals(Hello.Build.Version, expected, StringComparison.Ordinal))
        {
            throw new DedicatedServerStartException(Stop(new DsExit(DsExitKind.VersionMismatch, null, "ds.control.version_mismatch")));
        }
        LogLine(string.Create(CultureInfo.InvariantCulture, $"ds connected: pid={Pid} proto={Hello.Proto} version={_scrubber.Text(Hello.Build.Version)} supervision={Supervision}"));
    }

    /// <summary>After a start step failed on the launcher's side: kill and remember why.</summary>
    private DsExit Stop(DsExit why)
    {
        Kill(why);
        return why;
    }

    /// <summary>Waits for the child to exit on its own up to <paramref name="grace"/>, else kills it with <paramref name="fallback"/>.</summary>
    private async Task<DsExit> ExitWithinAsync(TimeSpan grace, DsExit fallback)
    {
        try
        {
            await _process.WaitForExitAsync().WaitAsync(grace).ConfigureAwait(false);
            return await ExitAsync().ConfigureAwait(false);
        }
        catch (TimeoutException)
        {
            return Stop(fallback) with { Code = await ExitCodeWithinAsync(DisposeBound).ConfigureAwait(false) };
        }
    }

    private async Task<DsExit> ExitAsync()
    {
        var code = await ExitCodeWithinAsync(DisposeBound).ConfigureAwait(false);
        return Volatile.Read(ref _launcherReason) is { } reason ? reason with { Code = code } : code is { } c ? DsExit.FromCode(c) : new DsExit(DsExitKind.Crashed, null, "no exit code");
    }

    private async Task<int?> ExitCodeWithinAsync(TimeSpan bound)
    {
        try
        {
            await _process.WaitForExitAsync().WaitAsync(bound).ConfigureAwait(false);
            return _process.ExitCode;
        }
        catch (Exception e) when (e is TimeoutException or InvalidOperationException or Win32Exception)
        {
            return null;
        }
    }

    /// <summary>Supervision: the channel's end or the process's exit, whichever first, ends every world (§2.3).</summary>
    private void Watch()
    {
        Volatile.Write(ref _watching, 1);
        _ = Task.Run(WatchAsync);
    }

    private async Task WatchAsync()
    {
        DsExit exit;
#pragma warning disable CA1031 // the supervision task always ends the stream, whatever went wrong
        try
        {
            var closed = Client.Closed;
            var exited = _process.WaitForExitAsync();
            await Task.WhenAny(closed, exited).ConfigureAwait(false);
            exit = exited.IsCompleted
                ? await ExitAsync().ConfigureAwait(false)
                : await ExitWithinAsync(EofGrace, new DsExit(DsExitKind.Crashed, null, "pipe eof without exit")).ConfigureAwait(false);
            // The exit can be seen before the reader has taken the child's last lines (its world.closed, a final diag)
            // out of the pipe: wait for the channel's own EOF, so every event the child wrote is dispatched before DsGone
            // ends the stream and the open-world list is final. The child is gone, so the pipe breaks and the read returns.
            if (await Task.WhenAny(closed, Task.Delay(EofGrace)).ConfigureAwait(false) != closed)
            {
                LogLine("ds ended: the control channel did not reach EOF in time");
            }
        }
        catch (Exception e)
        {
            exit = new DsExit(DsExitKind.Crashed, null, "supervision: " + e.GetType().Name);
        }
#pragma warning restore CA1031
        var worlds = Client.OpenWorlds.ToArray();
        LogLine(string.Create(CultureInfo.InvariantCulture, $"ds ended: pid={Pid} kind={exit.Kind} code={exit.Code?.ToString(CultureInfo.InvariantCulture) ?? "-"} worlds={worlds.Length}"));
        Client.Finish(new DsGone { Exit = exit, Worlds = worlds, Seq = 0 });
        _exited.TrySetResult(exit);
    }

    /// <summary>Asks for a clean stop (<c>shutdown</c>, §4.2) and waits up to <paramref name="grace"/>; then kills.</summary>
    public async Task<DsExit> ShutdownAsync(TimeSpan grace, CancellationToken cancellation = default)
    {
        try
        {
            await Client.ShutdownAsync(cancellation).WaitAsync(grace, cancellation).ConfigureAwait(false);
        }
        catch (Exception e) when (e is TimeoutException or DedicatedServerGoneException)
        {
            LogLine("ds shutdown: " + e.GetType().Name);
        }
        try
        {
            return await Exited.WaitAsync(grace, cancellation).ConfigureAwait(false);
        }
        catch (TimeoutException)
        {
            Kill(new DsExit(DsExitKind.Stopped, null, "shutdown timed out"));
        }
        try
        {
            return await Exited.WaitAsync(DisposeBound + EofGrace + EofGrace, cancellation).ConfigureAwait(false);
        }
        catch (TimeoutException)
        {
            // the outcome, never a TimeoutException: the child was killed and its end is still being supervised
            return Volatile.Read(ref _launcherReason) ?? new DsExit(DsExitKind.Stopped, null, "shutdown timed out");
        }
    }

    /// <summary>Kills the child (and anything it started) at once; its worlds are gone.</summary>
    public void Kill(DsExit why)
    {
        ArgumentNullException.ThrowIfNull(why);
        Interlocked.CompareExchange(ref _launcherReason, why, null);
        try
        {
            if (!_process.HasExited)
            {
                _process.Kill(entireProcessTree: true);
            }
        }
        catch (Exception e) when (e is InvalidOperationException or Win32Exception or NotSupportedException or AggregateException)
        {
            LogLine("ds kill: " + e.GetType().Name);
        }
    }

    /// <summary>Closes the job handle: with <c>KILL_ON_JOB_CLOSE</c> the system ends the child (what a crashed launcher gets).</summary>
    internal void CloseJob()
    {
        if (_job != IntPtr.Zero && Interlocked.Exchange(ref _jobClosed, 1) == 0)
        {
            Interlocked.CompareExchange(ref _launcherReason, new DsExit(DsExitKind.Stopped, null, "job closed"), null);
            Native.CloseHandle(_job);
        }
    }

    // ---- stdout / stderr ----

    private void StartReaders()
    {
        new Thread(ReadStdout) { IsBackground = true, Name = "fi-ds-stdout" }.Start();
        new Thread(ReadStderr) { IsBackground = true, Name = "fi-ds-stderr" }.Start();
    }

    private void ReadStdout()
    {
#pragma warning disable CA1031 // a reader thread never dies with an exception
        try
        {
            var framer = new DsLineFramer();
            var chunk = new byte[16 * 1024];
            var stream = _process.StandardOutput.BaseStream;
            var violation = false;
            int read;
            while (!violation && (read = stream.Read(chunk, 0, chunk.Length)) > 0)
            {
                if (!framer.Push(chunk.AsSpan(0, read), line =>
                {
                    if (!violation && !StdoutLine(line.Span))
                    {
                        violation = true;
                    }
                }))
                {
                    violation = true;
                    Kill(new DsExit(DsExitKind.ProtocolViolation, null, "stdout line over 1 MiB"));
                }
            }
            if (!violation && framer.Pending > 0)
            {
                StdoutLine(framer.TakePending()); // a last line without its newline is still one JSON line, or a violation
            }
            ReaderEnded("stdout", null);
        }
        catch (Exception e)
        {
            ReaderEnded("stdout", e);
        }
#pragma warning restore CA1031
    }

    /// <summary>
    /// A stdout/stderr reader ended. That is expected only because the child is exiting; a reader that stops (an
    /// exception, or EOF while the child goes on running) would leave the child blocked on a full pipe, so it is a
    /// violation kill, never a silent end.
    /// </summary>
    private void ReaderEnded(string stream, Exception? error)
    {
#pragma warning disable CA1031 // the check itself never throws out of a reader thread
        try
        {
            if (_process.WaitForExit(EofGrace))
            {
                return;
            }
            Kill(new DsExit(DsExitKind.ProtocolViolation, null, stream + " reader ended" + (error is null ? " (eof)" : ": " + error.GetType().Name)));
        }
        catch (Exception)
        {
            // the process object is gone: so is the child
        }
#pragma warning restore CA1031
    }

    /// <summary>One stdout line: a JSON object, scrubbed and forwarded (bounded); anything else kills the child (§2.3, treated as a crash).</summary>
    private bool StdoutLine(ReadOnlySpan<byte> line)
    {
        if (_scrubber.JsonLine(line) is not { } clean)
        {
            Kill(new DsExit(DsExitKind.ProtocolViolation, null, "stdout line not JSON"));
            return false;
        }
        Forward(DsLogScrubber.Cap("ds " + clean));
        return true;
    }

    private void ReadStderr()
    {
        // stderr is free-form and bounded (§2.3): at most 64 KiB and 64 lines forwarded, scrubbed and anonymised, never a kill
#pragma warning disable CA1031 // a reader thread never dies with an exception
        try
        {
            var framer = new DsLineFramer(MaxStderrBytes);
            var chunk = new byte[4096];
            var stream = _process.StandardError.BaseStream;
            var lines = 0;
            var total = 0;
            void Take(ReadOnlyMemory<byte> line)
            {
                if (lines++ < MaxStderrLines)
                {
                    Forward(DsLogScrubber.Cap("ds stderr: " + IntegratedHostRelay.Anonymous(_scrubber.Text(System.Text.Encoding.UTF8.GetString(line.Span)))));
                }
            }
            int read;
            while ((read = stream.Read(chunk, 0, chunk.Length)) > 0)
            {
                var keep = Math.Min(read, Math.Max(0, MaxStderrBytes - total));
                total += read;
                if (keep > 0 && !framer.Push(chunk.AsSpan(0, keep), Take))
                {
                    framer = new DsLineFramer(MaxStderrBytes);
                }
            }
            if (framer.Pending > 0)
            {
                Take(framer.TakePending()); // a final line without its newline
            }
            ReaderEnded("stderr", null);
        }
        catch (Exception e)
        {
            ReaderEnded("stderr", e);
        }
#pragma warning restore CA1031
    }

    private void Forward(string line)
    {
        if (!_log.Writer.TryWrite(line))
        {
            Interlocked.Increment(ref _logDropped);
        }
    }

    private void LogLine(string line) => Forward(line);

    private async Task PumpLogAsync()
    {
        await foreach (var line in _log.Reader.ReadAllAsync().ConfigureAwait(false))
        {
#pragma warning disable CA1031 // a throwing sink drops its line and the pump goes on
            try
            {
                _options.Log?.Invoke(line);
            }
            catch (Exception)
            {
                Interlocked.Increment(ref _logDropped);
            }
#pragma warning restore CA1031
        }
    }

    // ---- the job ----

    private static IntPtr AssignKillOnCloseJob(System.Diagnostics.Process process, out string? error)
    {
        error = null;
        var job = Native.CreateJobObjectW(IntPtr.Zero, null);
        if (job == IntPtr.Zero)
        {
            error = "CreateJobObjectW " + Marshal.GetLastPInvokeError().ToString(CultureInfo.InvariantCulture);
            return IntPtr.Zero;
        }
        var limits = new Native.ExtendedLimitInformation { BasicLimitInformation = new Native.BasicLimitInformation { LimitFlags = Native.JobObjectLimitKillOnJobClose } };
        if (!Native.SetInformationJobObject(job, Native.JobObjectExtendedLimitInformationClass, ref limits, Marshal.SizeOf<Native.ExtendedLimitInformation>()))
        {
            error = "SetInformationJobObject " + Marshal.GetLastPInvokeError().ToString(CultureInfo.InvariantCulture);
            Native.CloseHandle(job);
            return IntPtr.Zero;
        }
        if (!Native.AssignProcessToJobObject(job, process.SafeHandle))
        {
            error = "AssignProcessToJobObject " + Marshal.GetLastPInvokeError().ToString(CultureInfo.InvariantCulture);
            Native.CloseHandle(job);
            return IntPtr.Zero;
        }
        return job;
    }

    /// <summary>Ends the child at once (the job's kill-on-close, or a tree kill without one) and releases everything; every wait is bounded and asynchronous.</summary>
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _disposed, 1) != 0)
        {
            return;
        }
        Interlocked.CompareExchange(ref _launcherReason, new DsExit(DsExitKind.Stopped, null, "disposed"), null);
        if (_job != IntPtr.Zero)
        {
            CloseJob();
        }
        else
        {
            Kill(new DsExit(DsExitKind.Stopped, null, "disposed"));
        }
        if (await ExitCodeWithinAsync(DisposeBound).ConfigureAwait(false) is null)
        {
            Kill(new DsExit(DsExitKind.Stopped, null, "disposed"));
        }
        await Client.DisposeAsync().ConfigureAwait(false);
        _pipe.Dispose();
        if (Volatile.Read(ref _watching) == 0)
        {
            // never watched (a failed start): end the event stream here
            var exit = await ExitAsync().ConfigureAwait(false);
            Client.Finish(new DsGone { Exit = exit, Worlds = Client.OpenWorlds.ToArray() });
            _exited.TrySetResult(exit);
        }
        else
        {
            await Task.WhenAny(_exited.Task, Task.Delay(DisposeBound + EofGrace + EofGrace)).ConfigureAwait(false);
        }
        _log.Writer.TryComplete();
        if (_exited.Task.IsCompleted)
        {
            _exitedHandle.Dispose();
            _process.Dispose();
        }
    }

    private static class Native
    {
        public const int JobObjectExtendedLimitInformationClass = 9;
        public const uint JobObjectLimitKillOnJobClose = 0x00002000;

        [StructLayout(LayoutKind.Sequential)]
        public struct BasicLimitInformation
        {
            public long PerProcessUserTimeLimit;
            public long PerJobUserTimeLimit;
            public uint LimitFlags;
            public UIntPtr MinimumWorkingSetSize;
            public UIntPtr MaximumWorkingSetSize;
            public uint ActiveProcessLimit;
            public UIntPtr Affinity;
            public uint PriorityClass;
            public uint SchedulingClass;
        }

        [StructLayout(LayoutKind.Sequential)]
        public struct IoCounters
        {
            public ulong ReadOperationCount, WriteOperationCount, OtherOperationCount, ReadTransferCount, WriteTransferCount, OtherTransferCount;
        }

        [StructLayout(LayoutKind.Sequential)]
        public struct ExtendedLimitInformation
        {
            public BasicLimitInformation BasicLimitInformation;
            public IoCounters IoInfo;
            public UIntPtr ProcessMemoryLimit;
            public UIntPtr JobMemoryLimit;
            public UIntPtr PeakProcessMemoryUsed;
            public UIntPtr PeakJobMemoryUsed;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        public static extern IntPtr CreateJobObjectW(IntPtr attributes, string? name);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool SetInformationJobObject(IntPtr job, int infoClass, ref ExtendedLimitInformation info, int length);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool AssignProcessToJobObject(IntPtr job, SafeProcessHandle process);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool CloseHandle(IntPtr handle);
    }
}
