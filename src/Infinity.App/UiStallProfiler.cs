// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Collections.Concurrent;
using System.Diagnostics;
using System.Globalization;
using System.Reflection;
using System.Text;
using System.Windows;
using System.Windows.Input;
using System.Windows.Threading;

namespace Infinity.App;

/// <summary>
/// Owner 2026-11-10 ("NEVER [...] a freezed screen, even for 1s"): what the window's thread ran while the watchdog waited. On the UI
/// thread it times every dispatcher operation (<see cref="Dispatcher.Hooks"/>), every input event (a click runs in the window procedure,
/// not in an operation) and every "every property changed" notification of the view model, and keeps the slow ones (past
/// <see cref="SlowAfter"/>) in a small ring. The watchdog, once the thread answers, asks <see cref="Describe"/> for the slow pieces inside
/// its window. Only code names are kept (a delegate's method, an async method, a button's click handler), never text, paths or values.
/// The cost on the UI thread is two timestamps per operation; names are resolved only for the slow ones.
/// </summary>
internal sealed class UiStallProfiler
{
    private readonly record struct Slow(long Start, long End, string What);

    private readonly record struct Running(long Start, DispatcherOperation? Operation, InputEventArgs? Input);

    private readonly Lock _gate = new();
    private readonly Slow[] _ring = new Slow[256];
    private int _next;
    private readonly List<Running> _running = [];

    /// <summary>A piece of work this long or longer is remembered (the harness lowers it to see the small pieces that add up).</summary>
    internal static long SlowAfter { get; set; } = Stopwatch.Frequency * 30 / 1000;

    /// <summary>A notify-all this long or longer names its caller (a stack walk of the UI thread itself, only then).</summary>
    internal static readonly long NamedAfter = Stopwatch.Frequency * 15 / 1000;

    /// <summary>Every notify-all and named notification since the start (the harness reads the storm counts).</summary>
    internal long NotifyAllCount { get; private set; }

    internal long NamedCount { get; private set; }

    /// <summary>A handle on the UI thread (query only), for its CPU time from the watchdog's thread; zero when it could not be opened.</summary>
    private IntPtr _uiThread;

    /// <summary>The UI thread's CPU time so far (kernel + user, 100 ns units), from any thread; -1 when unknown.</summary>
    internal long UiCpuTicks() => _uiThread != IntPtr.Zero && GetThreadTimes(_uiThread, out _, out _, out var kernel, out var user) ? kernel + user : -1;

    [System.Runtime.InteropServices.DllImport("kernel32.dll")]
    [System.Runtime.InteropServices.DefaultDllImportSearchPaths(System.Runtime.InteropServices.DllImportSearchPath.System32)]
    private static extern uint GetCurrentThreadId();

    [System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
    [System.Runtime.InteropServices.DefaultDllImportSearchPaths(System.Runtime.InteropServices.DllImportSearchPath.System32)]
    private static extern IntPtr OpenThread(uint access, [System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)] bool inherit, uint threadId);

    [System.Runtime.InteropServices.DllImport("kernel32.dll", SetLastError = true)]
    [System.Runtime.InteropServices.DefaultDllImportSearchPaths(System.Runtime.InteropServices.DllImportSearchPath.System32)]
    [return: System.Runtime.InteropServices.MarshalAs(System.Runtime.InteropServices.UnmanagedType.Bool)]
    private static extern bool GetThreadTimes(IntPtr thread, out long creation, out long exit, out long kernel, out long user);

    /// <summary>Called on the UI thread: its dispatcher's operations, its input and its CPU time are watched from here on.</summary>
    internal void Attach(Dispatcher dispatcher)
    {
        ArgumentNullException.ThrowIfNull(dispatcher);
        // THREAD_QUERY_LIMITED_INFORMATION; kept for the process' lifetime (one handle); Wine answers or returns zero, both are fine
        try { _uiThread = OpenThread(0x0800, false, GetCurrentThreadId()); }
        catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException) { _uiThread = IntPtr.Zero; }
        dispatcher.Hooks.OperationStarted += (_, e) => Push(new Running(Stopwatch.GetTimestamp(), e.Operation, null));
        dispatcher.Hooks.OperationCompleted += (_, e) => EndOperation(e.Operation);
        dispatcher.Hooks.OperationAborted += (_, e) => EndOperation(e.Operation);
        InputManager.Current.PreProcessInput += (_, e) =>
        {
            if (e.StagingItem.Input is MouseButtonEventArgs or KeyEventArgs) Push(new Running(Stopwatch.GetTimestamp(), null, e.StagingItem.Input));
        };
        InputManager.Current.PostProcessInput += (_, e) =>
        {
            if (e.StagingItem.Input is not (MouseButtonEventArgs or KeyEventArgs)) return;
            var at = _running.FindLastIndex(r => ReferenceEquals(r.Input, e.StagingItem.Input));
            if (at < 0) return;
            var top = _running[at];
            _running.RemoveRange(at, _running.Count - at);
            var end = Stopwatch.GetTimestamp();
            if (end - top.Start >= SlowAfter) Remember(top.Start, end, "input " + e.StagingItem.Input.RoutedEvent?.Name + " on " + Element(e.StagingItem.Input.OriginalSource));
        };
    }

    private void Push(Running running)
    {
        if (_running.Count > 64) _running.RemoveRange(0, 32); // never grows: entries whose end was never raised are dropped
        _running.Add(running);
    }

    private void EndOperation(DispatcherOperation operation)
    {
        // nested frames (a PushFrame inside an operation) end inside out; an end without its start (an inline Invoke) changes nothing
        var at = _running.FindLastIndex(r => ReferenceEquals(r.Operation, operation));
        if (at < 0) return;
        var top = _running[at];
        _running.RemoveRange(at, _running.Count - at);
        var end = Stopwatch.GetTimestamp();
        if (end - top.Start >= SlowAfter) Remember(top.Start, end, Operation(operation));
    }

    /// <summary>A notify-all of the view model took from <paramref name="start"/> to now (called on the UI thread, right after it).</summary>
    internal void NotifiedAll(long start)
    {
        NotifyAllCount++;
        var end = Stopwatch.GetTimestamp();
        if (end - start < NamedAfter) return;
        Remember(start, end, "notify-all from " + Caller());
    }

    internal void Notified() => NamedCount++;

    /// <summary>Marks a named piece of work (a harness step, a measured block) as slow when it was.</summary>
    internal void Measured(long start, string what)
    {
        var end = Stopwatch.GetTimestamp();
        if (end - start >= SlowAfter) Remember(start, end, what);
    }

    private void Remember(long start, long end, string what)
    {
        lock (_gate)
        {
            _ring[_next] = new Slow(start, end, what);
            _next = (_next + 1) % _ring.Length;
        }
    }

    /// <summary>
    /// The slow pieces inside <paramref name="from"/>..<paramref name="to"/> (Stopwatch timestamps), longest first: "RefreshModsAsync 412 ms;
    /// notify-all from OpenProfiles 230 ms". Empty when none was recorded (the stall was outside the dispatcher, e.g. a GC or the OS).
    /// </summary>
    internal string Describe(long from, long to)
    {
        Slow[] inside;
        lock (_gate) inside = [.. _ring.Where(s => s.What is not null && s.End >= from && s.Start <= to)];
        if (inside.Length == 0) return string.Empty;
        var text = new StringBuilder();
        // the same piece many times (a card builder per row) is one entry with its count and total
        foreach (var group in inside.GroupBy(s => s.What, StringComparer.Ordinal).Select(g => (What: g.Key, Count: g.Count(), Ticks: g.Sum(s => s.End - s.Start)))
                     .OrderByDescending(g => g.Ticks).Take(6))
        {
            if (text.Length > 0) text.Append("; ");
            text.Append(group.What);
            if (group.Count > 1) text.Append(" x").Append(group.Count.ToString(CultureInfo.InvariantCulture));
            text.Append(' ').Append((group.Ticks * 1000 / Stopwatch.Frequency).ToString(CultureInfo.InvariantCulture)).Append(" ms");
        }
        return text.ToString();
    }

    // ------------------------------------------------------------------ names (only for slow pieces)

    private static readonly ConcurrentDictionary<Type, FieldInfo[]> s_fields = new();

    /// <summary>"ShellViewModel.RefreshModsAsync (await)", "LayoutManager.UpdateLayoutCallback", "MediaContext.RenderMessageHandler".</summary>
    internal static string Operation(DispatcherOperation operation)
    {
        try
        {
            var priority = operation.Priority.ToString();
            var found = new List<Delegate>();
            Collect(operation, found, 0);
            var ours = found.Select(Describe).FirstOrDefault(n => n.StartsWith("Infinity", StringComparison.Ordinal) || n.Contains("ShellViewModel", StringComparison.Ordinal)
                || n.Contains("MainWindow", StringComparison.Ordinal));
            var name = ours ?? (found.Count > 0 ? Describe(found[0]) : operation.GetType().Name);
            return name.Replace("Infinity.App.", "", StringComparison.Ordinal) + " [" + priority + "]";
        }
        catch (Exception e) when (e is FieldAccessException or TargetException or ArgumentException or InvalidOperationException) { return "operation [" + operation.Priority + "]"; }
    }

    private static void Collect(object? value, List<Delegate> found, int depth)
    {
        if (value is null || depth > 3 || found.Count > 8) return;
        if (value is Delegate d)
        {
            found.Add(d);
            if (d.Target is { } target && Wrapper(target.GetType())) Collect(target, found, depth + 1);
            return;
        }
        if (value is object[] array) { foreach (var item in array.Take(4)) Collect(item, found, depth + 1); return; }
        var type = value.GetType();
        // only the framework's own wrappers are opened (an operation, a callback holder); never our objects or a state machine
        if (type.IsPrimitive || value is string || !Wrapper(type)) return;
        foreach (var field in s_fields.GetOrAdd(type, Fields))
            Collect(field.GetValue(value), found, depth + 1);
    }

    private static bool Wrapper(Type type) => type.Namespace is { } ns && (ns.StartsWith("System", StringComparison.Ordinal) || ns.StartsWith("MS.", StringComparison.Ordinal))
        && !type.Name.StartsWith("AsyncStateMachineBox", StringComparison.Ordinal);

    private static FieldInfo[] Fields(Type type)
    {
        var fields = new List<FieldInfo>();
        for (var t = type; t is not null && t != typeof(object); t = t.BaseType)
            fields.AddRange(t.GetFields(BindingFlags.Instance | BindingFlags.NonPublic | BindingFlags.Public | BindingFlags.DeclaredOnly)
                .Where(f => !f.FieldType.IsValueType && f.FieldType != typeof(string) && f.FieldType != typeof(Dispatcher)
                    && !typeof(System.Threading.Tasks.Task).IsAssignableFrom(f.FieldType) && f.Name is not "_dispatcher" and not "_operation"));
        return [.. fields];
    }

    private static string Describe(Delegate d)
    {
        // an await's continuation: the async method the state machine stands for
        if (d.Target is { } target && target.GetType() is { IsGenericType: true } box && box.Name.StartsWith("AsyncStateMachineBox", StringComparison.Ordinal)
            && box.GetGenericArguments() is [.., var machine]) // AsyncTaskMethodBuilder<TResult>.AsyncStateMachineBox<TStateMachine>: the last one
            return AsyncName(machine) + " (await)";
        var method = d.Method;
        var owner = method.DeclaringType;
        if (owner is not null && owner.Name.StartsWith('<') && owner.DeclaringType is { } outer)
            return outer.FullName + "." + Lambda(owner.Name, method.Name);
        return (owner?.FullName ?? "?") + "." + Lambda(null, method.Name);
    }

    private static string AsyncName(Type machine)
    {
        var name = machine.Name; // <RefreshModsAsync>d__42
        var open = name.IndexOf('<', StringComparison.Ordinal); var close = name.IndexOf('>', StringComparison.Ordinal);
        var method = open >= 0 && close > open ? name[(open + 1)..close] : name;
        return (machine.DeclaringType?.FullName ?? "?") + "." + method;
    }

    private static string Lambda(string? display, string method)
    {
        // <OpenProfiles>b__12_0 → OpenProfiles (lambda)
        var open = method.IndexOf('<', StringComparison.Ordinal); var close = method.IndexOf('>', StringComparison.Ordinal);
        return open >= 0 && close > open ? method[(open + 1)..close] + " (lambda)" : method;
    }

    /// <summary>The view model's method that raised the notify-all (the first frame of ours above the notification).</summary>
    private static string Caller()
    {
        var frames = new StackTrace(2, false).GetFrames();
        var names = new List<string>();
        foreach (var frame in frames)
        {
            if (frame.GetMethod() is not { DeclaringType: { } type } method) continue;
            var outer = type.Name.StartsWith('<') && type.DeclaringType is { } declaring ? declaring : type;
            if (outer.Namespace != "Infinity.App" || outer == typeof(UiStallProfiler)) continue;
            var name = type.Name.StartsWith('<') ? AsyncName(type).Replace("Infinity.App.", "", StringComparison.Ordinal)
                : outer.Name + "." + Lambda(null, method.Name);
            if (name.EndsWith(".Changed", StringComparison.Ordinal)) continue;
            if (names.Count == 0 || names[^1] != name) names.Add(name);
            if (names.Count == 3) break;
        }
        return names.Count == 0 ? "?" : string.Join(" < ", names);
    }

    /// <summary>An input's element: its type, x:Name, and the click handler of the button it is in (a code name, never its text).</summary>
    private static string Element(object? source)
    {
        var node = source as DependencyObject;
        for (var depth = 0; node is not null && depth < 12; depth++)
        {
            if (node is System.Windows.Controls.Primitives.ButtonBase button)
                return button.GetType().Name + (button.Name.Length > 0 ? "#" + button.Name : "") + ClickHandler(button);
            node = System.Windows.Media.VisualTreeHelper.GetParent(node) ?? LogicalTreeHelper.GetParent(node);
        }
        return source is FrameworkElement element ? element.GetType().Name + (element.Name.Length > 0 ? "#" + element.Name : "") : source?.GetType().Name ?? "?";
    }

    private static readonly PropertyInfo? s_handlersStore = typeof(UIElement).GetProperty("EventHandlersStore", BindingFlags.Instance | BindingFlags.NonPublic);

    private static string ClickHandler(System.Windows.Controls.Primitives.ButtonBase button)
    {
        try
        {
            var store = s_handlersStore?.GetValue(button);
            var get = store?.GetType().GetMethod("GetRoutedEventHandlers", BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic, [typeof(RoutedEvent)]);
            if (get?.Invoke(store, [System.Windows.Controls.Primitives.ButtonBase.ClickEvent]) is RoutedEventHandlerInfo[] { Length: > 0 } handlers)
                return " (" + handlers[0].Handler.Method.Name + ")";
        }
        catch (Exception e) when (e is TargetInvocationException or ArgumentException or InvalidOperationException) { }
        return "";
    }
}
