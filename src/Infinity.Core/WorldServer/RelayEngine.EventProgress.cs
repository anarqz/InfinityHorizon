// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;

namespace Infinity.Core.WorldServer;

/// <summary>
/// "Event in Progress" for free-roam players outside a running event. SessionHostSynced (router 6) carries, besides the event flags,
/// var 2 CountdownType (2 bits: 0 none, 1 Launch, 2 StartRace), var 9 RaceStartTime (u32 network ms), var 10 NumLapsCompleted (u8)
/// and var 11 FurthestProgress (f32), the last two owned by the server alone. A bystander's event HUD says the race is in progress
/// only when CountdownType is 2 and 0 &lt; RaceStartTime &lt;= its clock ("about to start" before), and then shows "x % complete" from
/// FurthestProgress (under 2 laps; "Race complete" once NumLapsCompleted is not 0), or the lap NumLapsCompleted + 1 of the event's lap
/// count. The server works both out from the participants' scores: MsgInProgressScoreUpdate (43), each car's progress in laps, and
/// MsgLapScore (44), one per completed lap.
/// </summary>
internal sealed partial class RelayEngine
{
    internal const byte CountdownTypeVariable = 2, RaceStartTimeVariable = 9, NumLapsCompletedVariable = 10, FurthestProgressVariable = 11;
    internal const ulong CountdownStartRace = 2;
    internal const int CountdownTypeBits = 2, NumLapsCompletedBits = 8, FurthestProgressBits = 32;
    /// <summary>FurthestProgress is sent at most this often...</summary>
    internal const long FurthestProgressIntervalMs = 1000;
    /// <summary>...and only once it moved at least this much since the last value sent (0.5 % of the event).</summary>
    internal const float FurthestProgressMinDelta = 0.005f;
    /// <summary>GameOptions EndConditionType 1: laps. Another end condition's score is not a lap count.</summary>
    private const int LapsEndCondition = 1;

    private sealed class EventProgress(int laps)
    {
        internal int Laps { get; } = laps;
        /// <summary>Each participant's latest valid 43 score, in laps.</summary>
        internal Dictionary<byte, float> Progress { get; } = [];
        /// <summary>Each participant's 44 count, at most <see cref="Laps"/>.</summary>
        internal Dictionary<byte, int> LapsDone { get; } = [];
        /// <summary>High-water marks: neither value ever goes down during one event.</summary>
        internal int NumLapsCompleted { get; set; }
        internal float Furthest { get; set; }
        internal int SentLaps { get; set; }
        internal float SentFurthest { get; set; }
        internal long? SentFurthestAt { get; set; }
        /// <summary>The peers whose malformed 44 was already logged in this event.</summary>
        internal HashSet<byte> RefusedLaps { get; } = [];
    }

    private EventProgress? _eventProgress;
    /// <summary>Whether vars 10/11 were written during the current event, so its end clears them.</summary>
    private bool _eventProgressWritten;

    /// <summary>Forgets the event progress state (the engine's Clear).</summary>
    internal void EventProgressReset()
    {
        _eventProgress = null;
        _eventProgressWritten = false;
    }

    /// <summary>The race started: progress is followed for a lap race of known length (not a Road Trip's, nor a tag event).</summary>
    private void BeginEventProgress()
    {
        _eventProgress = null;
        string? off = null;
        if (IsRoadTrip) off = "a Road Trip event";
        else if (_tag is not null) off = "a tag event";
        else if (!GameOptionsFields.TryRead(_gameOptions.Records, out var fields)) off = "the options do not read";
        else if (fields.EndCondition is { } end && end != LapsEndCondition)
            off = string.Create(CultureInfo.InvariantCulture, $"end condition {end}");
        else if (fields.NumLaps is not (> 0 and <= byte.MaxValue and var laps)) off = "lap count unknown"; // NumLapsCompleted is a u8
        else _eventProgress = new((int)laps);
        Note(off is null
            ? string.Create(CultureInfo.InvariantCulture, $"event progress: followed over {_eventProgress!.Laps} lap(s)")
            : $"event progress: off ({off})");
    }

    /// <summary>A participant's 43: its f32 is the car's progress in laps, kept when finite and within 0..laps + 1 (sign dropped).</summary>
    private void EventProgressScore(RelayPeer peer, BitBuffer body)
    {
        if (_eventProgress is not { } progress || NerdPayload.InProgressScore(body) is not { } raw) return;
        var value = Math.Abs(raw);
        if (!float.IsFinite(value) || value > progress.Laps + 1) return;
        progress.Progress[peer.Index] = value;
        PublishEventProgress();
    }

    /// <summary>A participant's 44: one more lap done (at most the event's laps), when its body has the game's layout.</summary>
    private void EventProgressLap(RelayPeer peer, BitBuffer body)
    {
        if (_eventProgress is not { } progress) return;
        if (!NerdPayload.LapScoreOk(body))
        {
            if (progress.RefusedLaps.Add(peer.Index)) Note($"event progress: lap score from peer {peer.Index} refused (not the game's layout)");
            return;
        }
        progress.LapsDone[peer.Index] = Math.Min(progress.Laps, progress.LapsDone.GetValueOrDefault(peer.Index) + 1);
        PublishEventProgress();
    }

    /// <summary>A participant left the event: it no longer counts towards later maxima (the values already reached stay).</summary>
    private void EventProgressLeft(RelayPeer peer)
    {
        if (_eventProgress is not { } progress) return;
        progress.Progress.Remove(peer.Index);
        progress.LapsDone.Remove(peer.Index);
    }

    private void TickEventProgress() => PublishEventProgress();

    /// <summary>
    /// The maxima over the current participants, kept as high-water marks: NumLapsCompleted goes out at once when it changes;
    /// FurthestProgress at most once per <see cref="FurthestProgressIntervalMs"/>, after a change of <see cref="FurthestProgressMinDelta"/>,
    /// but at once when a completed lap lifts it (laps done / laps above the value sent) or when it reaches 100 %.
    /// </summary>
    private void PublishEventProgress()
    {
        if (_eventProgress is not { } progress || _race is not { Stage: RaceStage.Started } race) return;
        foreach (var index in race.Participants)
        {
            var laps = progress.LapsDone.GetValueOrDefault(index);
            var furthest = Math.Clamp(Math.Max(progress.Progress.GetValueOrDefault(index), laps) / progress.Laps, 0f, 1f);
            progress.NumLapsCompleted = Math.Max(progress.NumLapsCompleted, laps);
            progress.Furthest = Math.Max(progress.Furthest, furthest);
        }
        var updates = new List<(byte, ulong, int)>(2);
        if (progress.NumLapsCompleted != progress.SentLaps)
        {
            progress.SentLaps = progress.NumLapsCompleted;
            updates.Add((NumLapsCompletedVariable, (ulong)progress.NumLapsCompleted, NumLapsCompletedBits));
        }
        // a completed lap (the laps-done floor passing the value sent) and 100 % go out at once, whatever the throttle and delta
        var floor = Math.Min(1f, (float)progress.NumLapsCompleted / progress.Laps);
        var due = progress.Furthest > progress.SentFurthest
            && (progress.Furthest >= 1f || floor > progress.SentFurthest
                || progress.Furthest - progress.SentFurthest >= FurthestProgressMinDelta
                    && (progress.SentFurthestAt is not { } at || _now - at >= FurthestProgressIntervalMs));
        if (due)
        {
            progress.SentFurthest = progress.Furthest;
            progress.SentFurthestAt = _now;
            updates.Add((FurthestProgressVariable, NerdPayload.HostFloat(progress.Furthest), FurthestProgressBits));
        }
        if (updates.Count == 0) return;
        _eventProgressWritten = true;
        SetHostVariables([.. updates]);
    }

    /// <summary>
    /// The host variables the event's end clears besides the event flags and CountdownType: the start time, and the progress when
    /// some was sent (never written, both are still 0 everywhere).
    /// </summary>
    private (byte, ulong, int)[] EventProgressCleared()
    {
        _eventProgress = null;
        (byte, ulong, int)[] cleared = _eventProgressWritten
            ? [(RaceStartTimeVariable, 0, 32), (NumLapsCompletedVariable, 0, NumLapsCompletedBits), (FurthestProgressVariable, 0, FurthestProgressBits)]
            : [(RaceStartTimeVariable, 0, 32)];
        _eventProgressWritten = false;
        return cleared;
    }
}
