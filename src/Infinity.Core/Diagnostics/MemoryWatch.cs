// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using System.Runtime.InteropServices;

namespace Infinity.Core.Diagnostics;

/// <summary>
/// What the launcher's process holds now. <see cref="PrivateBytes"/> is everything committed for it; <see cref="GcCommitted"/> is the
/// part the .NET heap committed (<see cref="ManagedHeap"/>: what is allocated in it), so the rest is native (the renderer, the image
/// codecs, SQLite, the runtime itself). Numbers only: no path, name or address.
/// </summary>
public readonly record struct MemorySample(long PrivateBytes, long WorkingSet, long ManagedHeap, long GcCommitted, int Handles, int Threads,
    int GdiObjects, int UserObjects, int Gen2Collections)
{
    /// <summary>This process now. A count the OS does not give (Wine) reads -1.</summary>
    public static MemorySample Current()
    {
        using var process = System.Diagnostics.Process.GetCurrentProcess();
        int gdi = -1, user = -1;
        if (OperatingSystem.IsWindows())
        {
            try { gdi = (int)GetGuiResources(process.Handle, 0); user = (int)GetGuiResources(process.Handle, 1); }
            catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException or InvalidOperationException or System.ComponentModel.Win32Exception) { }
        }
        return new(process.PrivateMemorySize64, process.WorkingSet64, GC.GetTotalMemory(forceFullCollection: false),
            GC.GetGCMemoryInfo().TotalCommittedBytes, process.HandleCount, process.Threads.Count, gdi, user, GC.CollectionCount(2));
    }

    [DllImport("user32.dll", ExactSpelling = true)]
    [DefaultDllImportSearchPaths(DllImportSearchPath.System32)]
    private static extern uint GetGuiResources(IntPtr process, uint flags);
}

/// <summary>
/// T-1118: on 2026-11-18 a launcher left open since the day before held about 20 GB and launcher.log said nothing about it. The launcher
/// now writes its own memory to the log: one line at the start (the baseline), one every <see cref="ReportEvery"/>, and one
/// at once whenever the private bytes moved <see cref="GrowthStep"/> or more since the last line (read every <see cref="CheckEvery"/>).
/// Each line splits the managed heap from the rest and carries the handle, thread and window-object counts, so a growth is dated and
/// narrowed down by the log alone. At most a few lines an hour; never anything but numbers.
/// </summary>
public sealed class MemoryWatch : IDisposable
{
    public static readonly TimeSpan CheckEvery = TimeSpan.FromMinutes(1);
    public static readonly TimeSpan ReportEvery = TimeSpan.FromMinutes(30);

    /// <summary>A change of the private bytes this large since the last line is reported at the next check.</summary>
    public const long GrowthStep = 256L * 1024 * 1024;

    private readonly Func<MemorySample> _read;
    private readonly Action<string> _log;
    private readonly TimeProvider _clock;
    private readonly Lock _gate = new();
    private ITimer? _timer;
    private MemorySample? _reported;
    private long _reportedAt;

    public MemoryWatch(Action<string> log, Func<MemorySample>? read = null, TimeProvider? clock = null)
    {
        _log = log ?? throw new ArgumentNullException(nameof(log));
        _read = read ?? MemorySample.Current;
        _clock = clock ?? TimeProvider.System;
    }

    /// <summary>Starts the checks (once): the first at once (the baseline, on the timer's thread), then every <see cref="CheckEvery"/>.</summary>
    public void Start()
    {
        lock (_gate) _timer ??= _clock.CreateTimer(_ => Check(), null, TimeSpan.Zero, CheckEvery);
    }

    /// <summary>One check now: reads the process and logs the line when one is due.</summary>
    public void Check()
    {
        string? line;
        try { line = Observe(_read()); }
        catch (Exception e) when (e is InvalidOperationException or System.ComponentModel.Win32Exception or NotSupportedException) { return; }
        if (line is not null) _log(line);
    }

    /// <summary>The line to log for <paramref name="sample"/>, or null when none is due (the rules above, without I/O).</summary>
    public string? Observe(MemorySample sample)
    {
        lock (_gate)
        {
            var now = _clock.GetTimestamp();
            string? why = null;
            if (_reported is not { } last) why = "baseline";
            else
            {
                var moved = sample.PrivateBytes - last.PrivateBytes;
                var since = _clock.GetElapsedTime(_reportedAt, now);
                if (Math.Abs(moved) >= GrowthStep)
                    why = (moved > 0 ? "+" : "-") + Mb(Math.Abs(moved)) + " MB in " + ((long)since.TotalSeconds).ToString(CultureInfo.InvariantCulture) + " s";
                else if (since >= ReportEvery)
                    why = (moved >= 0 ? "+" : "-") + Mb(Math.Abs(moved)) + " MB in " + ((long)since.TotalMinutes).ToString(CultureInfo.InvariantCulture) + " min";
            }
            if (why is null) return null;
            _reported = sample;
            _reportedAt = now;
            static string Count(int value) => value < 0 ? "?" : value.ToString(CultureInfo.InvariantCulture);
            return "memory: private " + Mb(sample.PrivateBytes) + " MB (managed heap " + Mb(sample.ManagedHeap) + " MB, gc committed " + Mb(sample.GcCommitted)
                + " MB, native " + Mb(Math.Max(0, sample.PrivateBytes - sample.GcCommitted)) + " MB), working set " + Mb(sample.WorkingSet) + " MB, handles "
                + Count(sample.Handles) + ", threads " + Count(sample.Threads) + ", gdi " + Count(sample.GdiObjects) + ", user " + Count(sample.UserObjects)
                + ", gen2 " + Count(sample.Gen2Collections) + "; " + why;
        }
    }

    private static string Mb(long bytes) => (bytes / (1024 * 1024)).ToString(CultureInfo.InvariantCulture);

    public void Dispose()
    {
        lock (_gate) { _timer?.Dispose(); _timer = null; }
    }
}
