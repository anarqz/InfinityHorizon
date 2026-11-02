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
    /// <summary>Diagnostics, per event: at most this many var 10/11 updates and counted laps are logged...</summary>
    internal const int EventProgressUpdateNotes = 20, EventProgressLapNotes = 24;
    /// <summary>...and one line when no participant sent a 43 for this long while racing.</summary>
    internal const long EventProgressQuietMs = 30000;
    /// <summary>
    /// A 44 counts as a completed lap only when the racer's latest valid 43 progress (in laps) is at least its completed laps plus
    /// this: the game also sends a 44 on crossing the start line at progress about 0 (test.4 live log, 2026-11-02), which must not
    /// count, while a real lap ends with the progress near the next whole lap.
    /// </summary>
    internal const float LapScoreMinProgress = 0.5f;

    private sealed class EventProgress(int laps, long startedAt)
    {
        internal int Laps { get; } = laps;
        internal long StartedAt { get; } = startedAt;
        /// <summary>The last 43 from any participant (read or not), on the server clock.</summary>
        internal long? LastScoreAt { get; set; }
        internal bool QuietNoted { get; set; }
        internal HashSet<byte> FirstScoreNoted { get; } = [];
        internal HashSet<byte> RefusedScoreNoted { get; } = [];
        internal HashSet<byte> IgnoredScoreNoted { get; } = [];
        internal HashSet<byte> StartLineNoted { get; } = [];
        internal int UpdateNotes { get; set; }
        internal int LapNotes { get; set; }
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

    /// <summary>Forgets the event progress state and any held event end (the engine's Clear).</summary>
    internal void EventProgressReset()
    {
        _eventProgress = null;
        _eventProgressWritten = false;
        _eventHeld = false;
    }

    /// <summary>The race started: progress is followed for a lap race of known length (not a Road Trip's, nor a tag event).</summary>
    private void BeginEventProgress(uint start)
    {
        _eventProgress = null;
        // T24b: var 9 is on the network clock the games get from TimeSync and Pong (NowWire), as is the StartRace (40) they count down to
        Note(string.Create(CultureInfo.InvariantCulture, $"event progress: RaceStartTime {start} (network ms, now {NowWire})"));
        string? off = null;
        if (IsRoadTrip) off = "a Road Trip event";
        else if (_tag is not null) off = "a tag event";
        else if (!GameOptionsFields.TryRead(_gameOptions.Records, out var fields)) off = "the options do not read";
        else if (fields.EndCondition is { } end && end != LapsEndCondition)
            off = string.Create(CultureInfo.InvariantCulture, $"end condition {end}");
        else if (fields.NumLaps is not (> 0 and <= byte.MaxValue and var laps)) off = "lap count unknown"; // NumLapsCompleted is a u8
        else _eventProgress = new((int)laps, _now);
        Note(off is null
            ? string.Create(CultureInfo.InvariantCulture, $"event progress: followed over {_eventProgress!.Laps} lap(s)")
            : $"event progress: off ({off})");
    }

    /// <summary>A participant's 43: its f32 is the car's progress in laps, kept when finite and within 0..laps + 1 (sign dropped).</summary>
    private void EventProgressScore(RelayPeer peer, BitBuffer body)
    {
        if (_eventProgress is not { } progress) return;
        progress.LastScoreAt = _now;
        if (NerdPayload.InProgressScore(body) is not var (raw, flag))
        {
            RefusedScore(progress, peer, "?", "does not read");
            return;
        }
        var shown = raw.ToString("R", CultureInfo.InvariantCulture);
        if (progress.FirstScoreNoted.Add(peer.Index))
            Note(string.Create(CultureInfo.InvariantCulture, $"event progress: first 43 from peer {peer.Index}: {shown}, flag {(flag ? 1 : 0)}"));
        var value = Math.Abs(raw);
        if (!float.IsFinite(value)) { RefusedScore(progress, peer, shown, "not finite"); return; }
        if (value > progress.Laps + 1)
        {
            RefusedScore(progress, peer, shown, string.Create(CultureInfo.InvariantCulture, $"above {progress.Laps + 1} laps"));
            return;
        }
        progress.Progress[peer.Index] = value;
        PublishEventProgress();
    }

    /// <summary>A score the race does not take (a non-participant's, or not for the sender's own car): logged once per peer and event.</summary>
    private void EventProgressIgnored(RelayPeer peer, byte kind, string why)
    {
        if (kind is 43 or 44 && _eventProgress is { } progress && progress.IgnoredScoreNoted.Add(peer.Index))
            Note($"event progress: score {kind} from peer {peer.Index} ignored ({why})");
    }

    /// <summary>Logs a participant's first refused 43 of the event, with why.</summary>
    private void RefusedScore(EventProgress progress, RelayPeer peer, string value, string why)
    {
        if (progress.RefusedScoreNoted.Add(peer.Index)) Note($"event progress: 43 from peer {peer.Index} refused: {value} ({why})");
    }

    /// <summary>A participant's 44: one more lap done (at most the event's laps), when its body has the game's layout.</summary>
    private void EventProgressLap(RelayPeer peer, BitBuffer body)
    {
        if (_eventProgress is not { } progress) return;
        if (NerdPayload.LapScore(body) is not { } fields)
        {
            if (progress.RefusedLaps.Add(peer.Index)) Note($"event progress: lap score from peer {peer.Index} refused (not the game's layout)");
            return;
        }
        var done = progress.LapsDone.GetValueOrDefault(peer.Index);
        float? at = progress.Progress.TryGetValue(peer.Index, out var value) ? value : null;
        var shown = string.Create(CultureInfo.InvariantCulture,
            $"u32 {fields.Count8C}, f64 {fields.Value68:R}/{fields.Value78:R}/{fields.Value80:R}, flags {(fields.Flag89 ? 1 : 0)}{(fields.Flag8B ? 1 : 0)}");
        // T24c: the game also sends a 44 as its car crosses the start line, at progress about 0. A lap counts only once the car's own
        // progress (43, in laps) is at least half a lap past the laps it already completed
        if (at is not { } reached || reached < done + LapScoreMinProgress)
        {
            if (progress.StartLineNoted.Add(peer.Index))
                Note(string.Create(CultureInfo.InvariantCulture,
                    $"event progress: lap score at progress {(at is { } p ? p.ToString("R", CultureInfo.InvariantCulture) : "none")} not counted (start line) for peer {peer.Index}; {shown}"));
            return;
        }
        if (done >= progress.Laps) return; // every lap already counted
        progress.LapsDone[peer.Index] = done + 1;
        if (progress.LapNotes++ < EventProgressLapNotes)
            Note(string.Create(CultureInfo.InvariantCulture,
                $"event progress: lap {done + 1} of {progress.Laps} counted for peer {peer.Index} at progress {reached.ToString("R", CultureInfo.InvariantCulture)}; {shown}"));
        PublishEventProgress();
    }

    /// <summary>A participant left the event: it no longer counts towards later maxima (the values already reached stay).</summary>
    private void EventProgressLeft(RelayPeer peer)
    {
        if (_eventProgress is not { } progress) return;
        progress.Progress.Remove(peer.Index);
        progress.LapsDone.Remove(peer.Index);
    }

    private void TickEventProgress()
    {
        if (_eventProgress is { QuietNoted: false } progress && _race is { Stage: RaceStage.Started } race
            && _now - (progress.LastScoreAt ?? progress.StartedAt) >= EventProgressQuietMs)
        {
            progress.QuietNoted = true;
            Note(string.Create(CultureInfo.InvariantCulture,
                $"event progress: no 43 from any participant for {EventProgressQuietMs / 1000} s while racing ({race.Participants.Count} participant(s))"));
        }
        PublishEventProgress();
    }

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
        if (progress.UpdateNotes++ >= EventProgressUpdateNotes) return;
        var sent = string.Join(", ", updates.Select(u => u.Item1 == NumLapsCompletedVariable
            ? string.Create(CultureInfo.InvariantCulture, $"NumLapsCompleted {u.Item2}")
            : string.Create(CultureInfo.InvariantCulture, $"FurthestProgress {BitConverter.UInt32BitsToSingle((uint)u.Item2):0.000}")));
        Note($"event progress: sent {sent}"
            + (progress.UpdateNotes == EventProgressUpdateNotes ? " (later updates of this event are not logged)" : string.Empty));
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

    /// <summary>Every SessionHost variable an event's end clears: the event flags, the countdown, the start and the progress sent.</summary>
    private (byte, ulong, int)[] EventEndVariables() =>
        [(CountdownTypeVariable, 0, CountdownTypeBits), (4, 0, 1), (5, 0, 1), (6, 0, 1), .. EventProgressCleared()];

    /// <summary>The event flags (4 EventActive, 5 participants selected, 6 InRace), cleared at every event's end at once.</summary>
    private static readonly (byte, ulong, int)[] EventFlagsCleared = [(4, 0, 1), (5, 0, 1), (6, 0, 1)];

    /// <summary>The bystander HUD's variables (2, 9 and the progress sent), cleared at the end or when the "Race complete" hold ends.</summary>
    private (byte, ulong, int)[] HudCleared() => [(CountdownTypeVariable, 0, CountdownTypeBits), .. EventProgressCleared()];

    /// <summary>
    /// "Race complete" hold: the event closed while finishers are still on their results screens. The event flags 4, 5 and 6 are
    /// cleared at once, as before the hold: a finisher's game keeps its results screen (no button to go on) until they clear, so
    /// holding them deadlocked a lone finisher (test.4 live, 2026-11-02). Only the bystander HUD's vars 2, 9, 10 and 11 are held
    /// until the finishers are back in free roam: the HUD shows the completed state ("Race complete", 100 %, the last lap) only on
    /// its in-progress branch, which needs CountdownType 2 and a past RaceStartTime. Not for a Road Trip (its own state machine
    /// drives the next event, and everybody is in it) nor for an event that ended before its start (nobody is on a results screen).
    /// </summary>
    private bool _eventHeld;

    /// <summary>The event closed: true when the HUD variables are held (the caller then clears only the event flags).</summary>
    private bool HoldEventEnd(Race race)
    {
        if (race.Stage != RaceStage.Started || IsRoadTrip || !_phasing || _eventSide.Count == 0) return false;
        _eventProgress = null;
        _eventHeld = true;
        Note(string.Create(CultureInfo.InvariantCulture, $"event: HUD variables held while {_eventSide.Count} player(s) are on their results"));
        return true;
    }

    /// <summary>The hold ends (everybody back in free roam, the phasing backstop, or a new event): the HUD variables are cleared now.</summary>
    private void ReleaseEventHold(string why)
    {
        if (!_eventHeld) return;
        _eventHeld = false;
        Note($"event: HUD variables released ({why})");
        SetHostVariables(HudCleared());
    }
}
