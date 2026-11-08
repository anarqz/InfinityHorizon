// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using Infinity.Core.Saves;

namespace Infinity.Core.Shell;

/// <summary>The screens of the save manager (SAVE_MANAGER_UX.md §3, §4; the prototype's views).</summary>
public enum SavesScreen
{
    /// <summary>The root: both profile cards of the chosen game and the Azimuth strip.</summary>
    Home,

    /// <summary>A new profile (name, mode) for a mode the account has none of.</summary>
    Create,

    /// <summary>A profile with no career of the chosen game: Start fresh / from an existing profile / from this PC's save.</summary>
    Start,

    /// <summary>Cloud vs this PC, side by side.</summary>
    Resolve,

    /// <summary>The three slots (HARDCORE · Azimuth · NOOB) with Copy to… and History.</summary>
    Manage,

    /// <summary>Compare → (Check) → Confirm of one copy.</summary>
    Copy,

    /// <summary>One slot's timeline: this PC's history entries and the cloud's revisions.</summary>
    History,

    /// <summary>Delete a career or the whole profile, typed.</summary>
    Delete,

    /// <summary>"HELP SENSEI, I'M CONFUSED".</summary>
    Help,

    /// <summary>A finished action: "Back to Saves".</summary>
    Done,
}

/// <summary>
/// §3 "always a back button": the stack of save-manager screens. The root (<see cref="SavesScreen.Home"/>) has no back; every other screen
/// goes one step back (Esc, the mouse's back button, ‹ Back), never straight to the root, and nothing is written until a Confirm.
/// </summary>
public sealed class SavesNav
{
    private readonly List<SavesScreen> _stack = [SavesScreen.Home];

    public SavesScreen Current => _stack[^1];

    public bool CanGoBack => _stack.Count > 1;

    /// <summary>Root first: the breadcrumb ("Saves › Manage saves › Copy").</summary>
    public IReadOnlyList<SavesScreen> Trail => _stack;

    /// <summary>Opens <paramref name="screen"/> on top; the same screen again is not stacked twice, and Home resets to the root.</summary>
    public void Push(SavesScreen screen)
    {
        if (screen == SavesScreen.Home) { Root(); return; }
        if (Current == screen) return;
        // a screen already on the stack is returned to (History › Copy › History is History again, not a loop)
        var at = _stack.IndexOf(screen);
        if (at > 0) { _stack.RemoveRange(at + 1, _stack.Count - at - 1); return; }
        _stack.Add(screen);
    }

    /// <summary>One step back; false at the root (nothing to do).</summary>
    public bool Back()
    {
        if (!CanGoBack) return false;
        _stack.RemoveAt(_stack.Count - 1);
        return true;
    }

    public void Root()
    {
        _stack.Clear();
        _stack.Add(SavesScreen.Home);
    }
}

/// <summary>A profile card's plain-language sync status (SAVE_MANAGER_UX.md §2.3).</summary>
public enum SaveChipKind
{
    /// <summary>No career of the game (the card shows the Start choices instead).</summary>
    None,

    /// <summary>This PC and the cloud hold the same save: "Synced · 2 min ago".</summary>
    Synced,

    /// <summary>This PC moved since the last sync, the cloud did not.</summary>
    ThisPcNewer,

    /// <summary>The cloud moved since this PC last synced, this PC did not.</summary>
    CloudNewer,

    /// <summary>Both moved: the player chooses.</summary>
    BothChanged,

    /// <summary>A save on this PC, nothing in the cloud yet.</summary>
    NotSyncedYet,

    /// <summary>In the cloud, nothing on this PC: Download to this PC.</summary>
    CloudOnly,

    /// <summary>This PC's save and the cloud were never compared here (no mark): the next Play compares them.</summary>
    NotCompared,

    /// <summary>A game without cloud saves (FM5 v1): the save lives on this PC only.</summary>
    LocalOnly,

    /// <summary>A career with no save anywhere yet: it starts from the beginning.</summary>
    NewCareer,

    /// <summary>An accepted HARDCORE seed waits for the first Play (CR-S52).</summary>
    SeedPending,

    /// <summary>An upload is recorded and waits for the next Play.</summary>
    WaitingUpload,
}

/// <summary>How a chip is drawn: a green dot, an amber dot, or plain.</summary>
public enum ChipTone
{
    Neutral,
    Good,
    Warn,
}

/// <summary>What the chip of one card is computed from, all read on this PC (no request).</summary>
/// <param name="State">Where the career of the chosen game stands.</param>
/// <param name="CloudSaves">The game keeps cloud saves (<c>GameDefinition.CloudSaves</c>; FM5 too since CR-S105).</param>
/// <param name="CloudRevision">The career's newest cloud revision from <c>/me</c> (<c>career.cloud.revision</c>), null or 0 for none.</param>
/// <param name="LocalContent">The content digest of this PC's newest sealed save of the profile, null when this PC has none.</param>
/// <param name="Mark">What this PC last agreed with the cloud (sync-state), null when never.</param>
/// <param name="PendingUpload">An upload is recorded and not sent yet.</param>
/// <param name="SeedPending">An accepted seed waits for the first Play.</param>
public sealed record SaveChipInput(CareerState State, bool CloudSaves, long? CloudRevision, string? LocalContent, SyncMark? Mark,
    bool PendingUpload = false, bool SeedPending = false);

/// <summary>The one tested place for the cards' chips (§2.3) and the Resolve choice (§4 "Resolve").</summary>
public static class SaveChips
{
    public static SaveChipKind Of(SaveChipInput input)
    {
        ArgumentNullException.ThrowIfNull(input);
        if (input.State != CareerState.Started) return SaveChipKind.None;
        if (input.SeedPending) return SaveChipKind.SeedPending;
        var local = SaveReconcile.NormalizeDigest(input.LocalContent);
        if (!input.CloudSaves) return local is null && input.LocalContent is null ? SaveChipKind.NewCareer : SaveChipKind.LocalOnly;
        if (input.PendingUpload) return SaveChipKind.WaitingUpload;
        var cloud = input.CloudRevision is > 0 ? (ulong)input.CloudRevision.Value : 0UL;
        if (cloud == 0) return input.LocalContent is null ? SaveChipKind.NewCareer : SaveChipKind.NotSyncedYet;
        if (input.LocalContent is null) return SaveChipKind.CloudOnly;
        if (input.Mark is not { } mark || local is null) return SaveChipKind.NotCompared;
        var pcMoved = !string.Equals(local, SaveReconcile.NormalizeDigest(mark.Content), StringComparison.Ordinal);
        var cloudMoved = mark.Revision != cloud;
        return (pcMoved, cloudMoved) switch
        {
            (false, false) => SaveChipKind.Synced,
            (true, false) => SaveChipKind.ThisPcNewer,
            (false, true) => mark.Revision < cloud ? SaveChipKind.CloudNewer : SaveChipKind.NotCompared,
            (true, true) => SaveChipKind.BothChanged,
        };
    }

    public static ChipTone Tone(SaveChipKind kind) => kind switch
    {
        SaveChipKind.Synced => ChipTone.Good,
        SaveChipKind.ThisPcNewer or SaveChipKind.CloudNewer or SaveChipKind.BothChanged => ChipTone.Warn,
        _ => ChipTone.Neutral,
    };

    /// <summary>A chip that needs the player: its card's primary button is Resolve (§2.3).</summary>
    public static bool NeedsResolve(SaveChipKind kind) => kind is SaveChipKind.ThisPcNewer or SaveChipKind.CloudNewer or SaveChipKind.BothChanged;

    /// <summary>The cloud has something this PC lacks: "Download to this PC" is offered.</summary>
    public static bool CanDownload(SaveChipKind kind) => kind is SaveChipKind.CloudOnly or SaveChipKind.CloudNewer;

    /// <summary>
    /// The Resolve screen's pre-selected side: the side that moved, or (both moved) the one modified last; a tie keeps the cloud, which
    /// every other PC of the player sees too.
    /// </summary>
    public static ResolvePick Recommend(SaveChipKind kind, DateTimeOffset? cloudModified, DateTimeOffset? pcModified) => kind switch
    {
        SaveChipKind.ThisPcNewer => ResolvePick.ThisPc,
        SaveChipKind.CloudNewer or SaveChipKind.CloudOnly => ResolvePick.Cloud,
        _ => pcModified is { } pc && (cloudModified is not { } cloud || pc > cloud) ? ResolvePick.ThisPc : ResolvePick.Cloud,
    };
}

/// <summary>The Resolve screen's two sides.</summary>
public enum ResolvePick
{
    Cloud,
    ThisPc,
}

/// <summary>The three save slots of a game (§1).</summary>
public enum SaveSlot
{
    Hardcore,
    Azimuth,
    Noob,
}

/// <summary>Why a "Copy to…" destination is not offered as a copy (each shown disabled with its reason, §2.10).</summary>
public enum CopyBlock
{
    /// <summary>Offered: a copy (Compare → Check → Confirm).</summary>
    None,

    /// <summary>Offered as "Start the other profile from this": the destination has no career of the game yet (CAREERS §6.7).</summary>
    StartInstead,

    /// <summary>The game is running: Azimuth's save is not written now.</summary>
    GameRunning,

    /// <summary>This PC has no save of the source to copy.</summary>
    NoSource,

    /// <summary>The account has no profile of the destination's mode.</summary>
    NoProfile,

    /// <summary>A copy into an existing cloud career: no route yet (CAREERS §6.13, needs MACBOOK).</summary>
    NeedsServer,

    /// <summary>The game keeps no cloud saves (FM5 v1).</summary>
    NoCloudSaves,

    /// <summary>The game allows no HARDCORE career (FM5 until M11, §6.2).</summary>
    NotOffered,

    /// <summary>HARDCORE never starts from this PC's own Azimuth save (B2); only the other profile's career, through the check.</summary>
    NoHardcoreFromLocal,

    /// <summary>The server does not allow a HARDCORE career from a profile (<c>saves.hardcoreFromProfile</c> off, or no cloud line).</summary>
    CheckOff,
}

/// <summary>One destination of "Copy to…".</summary>
public sealed record CopyTarget(SaveSlot To, CopyBlock Block)
{
    public bool Enabled => Block is CopyBlock.None or CopyBlock.StartInstead;
}

/// <summary>What the slots hold on this PC and the account, for <see cref="CopyTargets.Of"/>.</summary>
public sealed record SlotFacts(
    bool HardcoreProfile,
    bool HardcoreCareer,
    bool HardcoreLocal,
    bool NoobProfile,
    bool NoobCareer,
    bool NoobLocal,
    bool AzimuthLocal,
    bool GameRunning,
    bool CloudSaves,
    bool HardcoreCareers,
    bool HardcoreFromProfile);

/// <summary>
/// "Copy to…" (§2.1): the destinations of a slot, the valid ones enabled. Only Azimuth (this PC) is written by a copy today; a cloud career
/// that exists cannot be written by the launcher until the server's copy route exists (CAREERS §6.13), and a cloud career that does not
/// exist yet is started from the source instead (CR-S52 for HARDCORE, the OD-41 picker for NOOB).
/// </summary>
public static class CopyTargets
{
    public static IReadOnlyList<CopyTarget> Of(SaveSlot from, SlotFacts f)
    {
        ArgumentNullException.ThrowIfNull(f);
        var list = new List<CopyTarget>(2);
        foreach (var to in new[] { SaveSlot.Hardcore, SaveSlot.Azimuth, SaveSlot.Noob })
        {
            if (to == from) continue;
            list.Add(new CopyTarget(to, Block(from, to, f)));
        }
        return list;
    }

    private static bool HasLocal(SaveSlot slot, SlotFacts f) => slot switch
    {
        SaveSlot.Hardcore => f.HardcoreLocal,
        SaveSlot.Noob => f.NoobLocal,
        _ => f.AzimuthLocal,
    };

    private static bool HasCareer(SaveSlot slot, SlotFacts f) => slot == SaveSlot.Hardcore ? f.HardcoreCareer : f.NoobCareer;

    private static CopyBlock Block(SaveSlot from, SaveSlot to, SlotFacts f)
    {
        if (to == SaveSlot.Azimuth)
        {
            if (f.GameRunning) return CopyBlock.GameRunning;
            return HasLocal(from, f) ? CopyBlock.None : CopyBlock.NoSource;
        }
        if (!(to == SaveSlot.Hardcore ? f.HardcoreProfile : f.NoobProfile)) return CopyBlock.NoProfile;
        if (to == SaveSlot.Hardcore && !f.HardcoreCareers) return CopyBlock.NotOffered;
        if (!f.CloudSaves) return CopyBlock.NoCloudSaves;
        if (HasCareer(to, f)) return CopyBlock.NeedsServer;
        if (to == SaveSlot.Hardcore)
        {
            if (from == SaveSlot.Azimuth) return CopyBlock.NoHardcoreFromLocal;
            if (!f.HardcoreFromProfile) return CopyBlock.CheckOff;
            return f.NoobCareer ? CopyBlock.StartInstead : CopyBlock.NoSource;
        }
        if (f.GameRunning) return CopyBlock.GameRunning;
        return HasLocal(from, f) ? CopyBlock.StartInstead : CopyBlock.NoSource;
    }
}

/// <summary>One line of the HARDCORE check's step list (§2.5).</summary>
public enum CheckStepState
{
    Waiting,
    Running,
    Done,
    Failed,
}

/// <summary>§2.5 "Reading save → Checking progression → Result", from the dry run's state.</summary>
public static class HardcoreCheck
{
    /// <param name="outcome">The dry run's answer, null while it runs (or before it ran).</param>
    /// <param name="running">The request is in flight.</param>
    /// <param name="downloading">OD-216: the source's latest cloud revision is being downloaded (the first step runs).</param>
    public static IReadOnlyList<CheckStepState> Steps(SeedCheckOutcome? outcome, bool running, bool downloading = false) => outcome switch
    {
        null when downloading => [CheckStepState.Running, CheckStepState.Waiting, CheckStepState.Waiting],
        null when running => [CheckStepState.Done, CheckStepState.Running, CheckStepState.Waiting],
        null => [CheckStepState.Waiting, CheckStepState.Waiting, CheckStepState.Waiting],
        SeedCheckOutcome.Accepted => [CheckStepState.Done, CheckStepState.Done, CheckStepState.Done],
        SeedCheckOutcome.Refused or SeedCheckOutcome.SourceUsed => [CheckStepState.Done, CheckStepState.Done, CheckStepState.Failed],
        _ => [CheckStepState.Done, CheckStepState.Failed, CheckStepState.Waiting], // nothing was decided (rate limit, server, network)
    };

    /// <summary>A refusal offers its ways out (§2.5): "Start a fresh HARDCORE career" and "Play it as NOOB".</summary>
    public static bool OffersWaysOut(SeedCheckOutcome? outcome) => outcome is SeedCheckOutcome.Refused or SeedCheckOutcome.SourceUsed;
}

/// <summary>§2.4 / §4 "Delete profile": the typed confirmation.</summary>
public static class DeleteConfirm
{
    /// <summary>The player typed the gamertag (trimmed, case-insensitive as the gamertag rules are); an empty name never matches.</summary>
    public static bool Matches(string? typed, string? gamertag) =>
        !string.IsNullOrWhiteSpace(gamertag) && string.Equals((typed ?? string.Empty).Trim(), gamertag.Trim(), StringComparison.OrdinalIgnoreCase);
}
