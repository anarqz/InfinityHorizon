// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.ComponentModel;
using System.Diagnostics;
using System.Globalization;

namespace Infinity.Core.DedicatedServer;

/// <summary>
/// <c>dedicated-server --probe-bind &lt;port&gt;</c> (DEDICATED_SERVER.md §2.4): run once after an update brings a new exe
/// and on the first Host, while the game is not running, so the first-bind firewall prompt comes at a calm moment.
/// The probe holds the port for 1 s; the launcher waits at most <see cref="DefaultTimeout"/> (the prompt does not block
/// the bind) and kills it past that.
/// </summary>
public static class DsProbeRunner
{
    public static readonly TimeSpan DefaultTimeout = TimeSpan.FromSeconds(15);

    public static IReadOnlyList<string> Arguments(int port) =>
        port is > 0 and <= 65535 ? ["--probe-bind", port.ToString(CultureInfo.InvariantCulture)] : throw new ArgumentOutOfRangeException(nameof(port));

    public static Task<DsProbeResult> RunAsync(string executable, int port, TimeSpan? timeout = null, CancellationToken cancellation = default) =>
        RunAsync(executable, port, null, timeout, cancellation);

    /// <summary>As above; with <paramref name="signature"/>, the exe runs only when it verifies, held open until it has started (<see cref="DsSignature.Open"/>).</summary>
    public static async Task<DsProbeResult> RunAsync(string executable, int port, DsSignaturePolicy? signature, TimeSpan? timeout = null, CancellationToken cancellation = default)
    {
        ArgumentNullException.ThrowIfNull(executable);
        var args = Arguments(port);
        if (!Path.IsPathFullyQualified(executable) || !File.Exists(executable))
        {
            return DsProbeResult.NotStarted;
        }
        DsVerifiedExecutable? verified = null;
        if (signature is not null && (verified = DsSignature.Open(executable, signature)) is null)
        {
            return DsProbeResult.NotStarted;
        }
        using var held = verified;
        var start = new ProcessStartInfo(executable)
        {
            UseShellExecute = false,
            CreateNoWindow = true,
            RedirectStandardInput = true,
            RedirectStandardOutput = true,
            RedirectStandardError = true,
            WorkingDirectory = Path.GetDirectoryName(executable)!,
        };
        foreach (var arg in args)
        {
            start.ArgumentList.Add(arg);
        }
        foreach (var name in start.Environment.Keys.Where(k => k.StartsWith(DsProto.EnvPrefix, StringComparison.OrdinalIgnoreCase)).ToArray())
        {
            start.Environment.Remove(name);
        }
        System.Diagnostics.Process? process;
        try
        {
            process = System.Diagnostics.Process.Start(start);
        }
        catch (Exception e) when (e is Win32Exception or InvalidOperationException)
        {
            return DsProbeResult.NotStarted;
        }
        if (process is null)
        {
            return DsProbeResult.NotStarted;
        }
        using (process)
        {
            process.StandardInput.Close();
            // drained, never read: a probe's output is not needed and must not block it
            _ = process.StandardOutput.BaseStream.CopyToAsync(Stream.Null, cancellation);
            _ = process.StandardError.BaseStream.CopyToAsync(Stream.Null, cancellation);
            try
            {
                await process.WaitForExitAsync(cancellation).WaitAsync(timeout ?? DefaultTimeout, cancellation).ConfigureAwait(false);
            }
            catch (TimeoutException)
            {
                Kill(process);
                return DsProbeResult.TimedOut;
            }
            catch (OperationCanceledException)
            {
                Kill(process);
                throw;
            }
            return DsProbe.FromExitCode(process.ExitCode);
        }
    }

    private static void Kill(System.Diagnostics.Process process)
    {
        try
        {
            process.Kill();
        }
        catch (Exception e) when (e is InvalidOperationException or Win32Exception)
        {
            // it ended meanwhile
        }
    }
}
