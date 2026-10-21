// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.ComponentModel;
using System.IO.Pipes;
using System.Runtime.InteropServices;
using System.Runtime.Versioning;
using System.Security.Principal;
using Microsoft.Win32.SafeHandles;
using Infinity.Core.Wire;

namespace Infinity.Core.Windows;

/// <summary>The bounded accept's outcome (ORCHESTRATOR §1.4).</summary>
public enum AcceptResult
{
    /// <summary>The child connected (its PID checked).</summary>
    Connected,

    /// <summary>The game exited before connecting: supervise its exit as usual.</summary>
    ChildExited,

    /// <summary>No valid client within this call's bound: call again while the child lives.</summary>
    TimedOut,
}

/// <summary>
/// The named-pipe server of both pipes (session.rs <c>pipe::PipeServer</c>; works inside Wine too,
/// where the hook's <c>CreateFileW</c> reaches the same wineserver):
/// <list type="bullet">
/// <item>a protected DACL granting the current user's SID only (<c>D:P(A;;GA;;;&lt;SID&gt;)</c>),
/// <c>FILE_FLAG_FIRST_PIPE_INSTANCE</c>, one instance, <c>PIPE_REJECT_REMOTE_CLIENTS</c>, byte mode;</item>
/// <item>no unbounded waits: <see cref="AcceptAsync"/> returns on connect, on the child's exit
/// or at its timeout, and a client whose PID is not the child is disconnected and the connect
/// re-armed;</item>
/// <item>writes bounded by <see cref="WriteBound"/>, so a peer that stops reading never stops the
/// heartbeats, the shutdown or the leave.</item>
/// </list>
/// </summary>
[SupportedOSPlatform("windows")]
public sealed class PipeServer : IDisposable
{
    /// <summary>The longest one write may wait for the peer to drain the pipe.</summary>
    public static readonly TimeSpan WriteBound = TimeSpan.FromSeconds(2);

    private readonly NamedPipeServerStream _stream;
    private readonly SemaphoreSlim _writes = new(1,1);

    private PipeServer(string name, NamedPipeServerStream stream)
    {
        Name = name;
        _stream = stream;
    }

    public string Name { get; }

    /// <summary>The connected stream, for the reader thread (<see cref="Frames.ReadAll{T}"/>).</summary>
    public Stream Stream => _stream;

    public bool IsConnected => _stream.IsConnected;

    /// <summary>Creates the pipe <paramref name="name"/> (<c>\\.\pipe\ForzaInfinity-&lt;nonce&gt;[-ui]</c>) before the child exists.</summary>
    public static PipeServer Create(string name)
    {
        ArgumentNullException.ThrowIfNull(name);
        using var identity = WindowsIdentity.GetCurrent();
        var sid = identity.User?.Value ?? throw new InvalidOperationException("no user SID");
        if (!NativeMethods.ConvertStringSecurityDescriptorToSecurityDescriptorW($"D:P(A;;GA;;;{sid})", 1, out var descriptor, IntPtr.Zero))
        {
            throw new Win32Exception(Marshal.GetLastPInvokeError(), "ConvertStringSecurityDescriptorToSecurityDescriptorW");
        }
        SafePipeHandle handle;
        try
        {
            var attributes = new NativeMethods.SecurityAttributes
            {
                Length = Marshal.SizeOf<NativeMethods.SecurityAttributes>(),
                SecurityDescriptor = descriptor,
                InheritHandle = 0,
            };
            handle = NativeMethods.CreateNamedPipeW(
                name,
                NativeMethods.PipeAccessDuplex | NativeMethods.FileFlagFirstPipeInstance | NativeMethods.FileFlagOverlapped,
                NativeMethods.PipeTypeByte | NativeMethods.PipeReadModeByte | NativeMethods.PipeWait | NativeMethods.PipeRejectRemoteClients,
                1,
                Frames.MaxFrame,
                Frames.MaxFrame,
                0,
                ref attributes);
        }
        finally
        {
            NativeMethods.LocalFree(descriptor);
        }
        if (handle.IsInvalid)
        {
            var error = Marshal.GetLastPInvokeError();
            handle.Dispose();
            throw new Win32Exception(error, $"CreateNamedPipeW {name}");
        }
        try
        {
            return new PipeServer(name, new NamedPipeServerStream(PipeDirection.InOut, isAsync: true, isConnected: false, handle));
        }
        catch
        {
            handle.Dispose();
            throw;
        }
    }

    /// <summary>
    /// Waits up to <paramref name="timeout"/> for the child <paramref name="childPid"/> to connect,
    /// returning early when <paramref name="childExit"/> (its process handle) is signalled. The
    /// supervisor calls it again for the child's whole life: the hook loads only after the player
    /// presses Start in Azimuth's window (ORCHESTRATOR §1.4).
    /// </summary>
    public async Task<AcceptResult> AcceptAsync(uint childPid, WaitHandle childExit, TimeSpan timeout, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(childExit);
        var deadline = DateTime.UtcNow + timeout;
        // A TaskCompletionSource, not a CancellationTokenSource: a wait callback still in flight
        // after Unregister can complete it safely (nothing here is disposed under it).
        var exited = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
        var registration = ThreadPool.RegisterWaitForSingleObject(childExit, (_, _) => exited.TrySetResult(), null, Timeout.Infinite, executeOnlyOnce: true);
        try
        {
            while (true)
            {
                // As session.rs: the connect is tried first, so a client already connected wins
                // over a zero timeout and over a child that has just exited.
                var left = deadline - DateTime.UtcNow;
                using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
                bound.CancelAfter(left > TimeSpan.Zero ? left : TimeSpan.FromMilliseconds(1));
                var connect = _stream.WaitForConnectionAsync(bound.Token);
                if (!connect.IsCompleted && await Task.WhenAny(connect, exited.Task).ConfigureAwait(false) == exited.Task)
                {
                    await bound.CancelAsync().ConfigureAwait(false);
                }
                try
                {
                    await connect.ConfigureAwait(false);
                }
                catch (OperationCanceledException)
                {
                    cancellation.ThrowIfCancellationRequested();
                    return exited.Task.IsCompleted ? AcceptResult.ChildExited : AcceptResult.TimedOut;
                }
                if (NativeMethods.GetNamedPipeClientProcessId(_stream.SafePipeHandle, out var pid) && pid == childPid)
                {
                    return AcceptResult.Connected;
                }
                // Someone else got there first: drop them and wait again while time remains.
                _stream.Disconnect();
                if (exited.Task.IsCompleted)
                {
                    return AcceptResult.ChildExited;
                }
                if (DateTime.UtcNow >= deadline)
                {
                    return AcceptResult.TimedOut;
                }
            }
        }
        finally
        {
            registration.Unregister(null);
        }
    }

    /// <summary>Writes one encoded frame within <see cref="WriteBound"/>; past it the write is cancelled and <see cref="TimeoutException"/> thrown.</summary>
    public async Task WriteAsync(byte[] frame, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(frame);
        using var bound = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
        bound.CancelAfter(WriteBound);
        var entered=false;
        try
        {
            await _writes.WaitAsync(bound.Token).ConfigureAwait(false);entered=true;
            await _stream.WriteAsync(frame, bound.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (!cancellation.IsCancellationRequested)
        {
            throw new TimeoutException("the peer did not read the pipe in time");
        }
        finally { if(entered)_writes.Release(); }
    }

    public void Dispose()
    {
        if (_stream.IsConnected)
        {
            try
            {
                _stream.Disconnect();
            }
            catch (IOException)
            {
                // The peer already went away.
            }
        }
        _stream.Dispose();
    }

    private static class NativeMethods
    {
        public const uint PipeAccessDuplex = 0x00000003;
        public const uint FileFlagFirstPipeInstance = 0x00080000;
        public const uint FileFlagOverlapped = 0x40000000;
        public const uint PipeTypeByte = 0x00000000;
        public const uint PipeReadModeByte = 0x00000000;
        public const uint PipeWait = 0x00000000;
        public const uint PipeRejectRemoteClients = 0x00000008;

        [StructLayout(LayoutKind.Sequential)]
        public struct SecurityAttributes
        {
            public int Length;
            public IntPtr SecurityDescriptor;
            public int InheritHandle;
        }

        [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        public static extern SafePipeHandle CreateNamedPipeW(
            string name, uint openMode, uint pipeMode, uint maxInstances, uint outBufferSize, uint inBufferSize, uint defaultTimeout, ref SecurityAttributes attributes);

        [DllImport("kernel32.dll", SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool GetNamedPipeClientProcessId(SafePipeHandle pipe, out uint clientProcessId);

        [DllImport("advapi32.dll", CharSet = CharSet.Unicode, SetLastError = true, ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        [return: MarshalAs(UnmanagedType.Bool)]
        public static extern bool ConvertStringSecurityDescriptorToSecurityDescriptorW(string sddl, uint revision, out IntPtr descriptor, IntPtr descriptorSize);

        [DllImport("kernel32.dll", ExactSpelling = true)]
        [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
        public static extern IntPtr LocalFree(IntPtr memory);
    }
}
