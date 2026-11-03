// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// Written for Infinity; fh2-mods (conner-mcvay, MIT) used as inspiration and reference; no code copied.
//! Synced-variable encodings (§5) and object payload variables (§4.5).
//!
//! An [`ObjectSchema`] is one object type's variable list (spec §7): it decodes and encodes a
//! payload's `(variable id, value)` pairs. [`object_schema`] returns the lists §7 gives so far.

use super::{
    CodecError,
    bits::{BitReader, BitRun, BitWriter, bit_length},
};

/// The wire format of one synced variable (§5). Values are raw: a template's range minimum is
/// not subtracted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarType {
    /// `SyncedInt<unsigned T, …, bits>` or `SyncedEnum<E, default, bits>`: `bits` bits unsigned
    /// (`1..=32`).
    UInt { bits: u8 },
    /// `SyncedInt<signed T, …, bits>`: two's complement (§1.4), `2..=32` bits.
    SInt { bits: u8 },
    /// `SyncedInt<float, …, 32>`: raw IEEE-754 single.
    F32,
    /// `SyncedInt<64-bit T, …>`: 8 bytes, least significant first.
    U64,
    /// `SyncedBool`: 1 bit.
    Bool,
    /// `SyncedArray<T, Range, count, bits>`: `count` elements, all written every time, each in
    /// the scalar format `elem` (`UInt`, `SInt`, `F32`, `U64` or `Bool`).
    Array { elem: Box<VarType>, count: u16 },
    /// `SyncedString<wchar_t, max>`: length in `bits(max)` bits (rejected above `max`), then
    /// that many 16-bit UTF-16 code units.
    WString { max: u16 },
    /// `SyncedString<char, max>`: length in `bits(max)` bits (rejected above `max`), then that
    /// many 8-bit characters.
    CString { max: u16 },
}

/// A decoded variable value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VarValue {
    UInt(u32),
    SInt(i32),
    /// The raw IEEE-754 bit pattern (exact round trips, NaN payloads included).
    F32(u32),
    U64(u64),
    Bool(bool),
    Array(Vec<VarValue>),
    WString(Vec<u16>),
    CString(Vec<u8>),
}

impl VarType {
    /// Checks the type's parameters (widths, scalar array elements, a non-zero string maximum).
    pub fn validate(&self) -> Result<(), CodecError> {
        match self {
            VarType::UInt { bits } if (1..=32).contains(bits) => Ok(()),
            VarType::SInt { bits } if (2..=32).contains(bits) => Ok(()),
            VarType::F32 | VarType::U64 | VarType::Bool => Ok(()),
            VarType::Array { elem, .. } => match **elem {
                VarType::Array { .. } | VarType::WString { .. } | VarType::CString { .. } => {
                    Err(CodecError::Layout)
                }
                ref e => e.validate(),
            },
            VarType::WString { max } | VarType::CString { max } if *max > 0 => Ok(()),
            _ => Err(CodecError::Layout),
        }
    }

    fn len_bits(max: u16) -> u8 {
        bit_length(u32::from(max))
    }

    /// Reads one value.
    pub fn read(&self, r: &mut BitReader<'_>) -> Result<VarValue, CodecError> {
        Ok(match self {
            VarType::UInt { bits } => VarValue::UInt(r.read(*bits)?),
            VarType::SInt { bits } => VarValue::SInt(r.read_signed(*bits)?),
            VarType::F32 => VarValue::F32(r.read_f32_bits()?),
            VarType::U64 => VarValue::U64(r.read_u64()?),
            VarType::Bool => VarValue::Bool(r.read_bool()?),
            VarType::Array { elem, count } => {
                self.validate()?;
                let mut v = Vec::with_capacity(usize::from(*count).min(r.remaining()));
                for _ in 0..*count {
                    v.push(elem.read(r)?);
                }
                VarValue::Array(v)
            }
            VarType::WString { max } => {
                let len = r.read(Self::len_bits(*max))? as u16;
                if len > *max {
                    return Err(CodecError::TooLong {
                        len: usize::from(len),
                        max: usize::from(*max),
                    });
                }
                let mut s = Vec::with_capacity(usize::from(len));
                for _ in 0..len {
                    s.push(r.read(16)? as u16);
                }
                VarValue::WString(s)
            }
            VarType::CString { max } => {
                let len = r.read(Self::len_bits(*max))? as u16;
                if len > *max {
                    return Err(CodecError::TooLong {
                        len: usize::from(len),
                        max: usize::from(*max),
                    });
                }
                let mut s = Vec::with_capacity(usize::from(len));
                for _ in 0..len {
                    s.push(r.read(8)? as u8);
                }
                VarValue::CString(s)
            }
        })
    }

    /// Writes one value; `Err(Layout)` when the value is not of this type.
    pub fn write(&self, v: &VarValue, w: &mut BitWriter) -> Result<(), CodecError> {
        match (self, v) {
            (VarType::UInt { bits }, VarValue::UInt(x)) => w.write(*x, *bits),
            (VarType::SInt { bits }, VarValue::SInt(x)) => w.write_signed(*x, *bits),
            (VarType::F32, VarValue::F32(x)) => w.write_f32_bits(*x),
            (VarType::U64, VarValue::U64(x)) => w.write_u64(*x),
            (VarType::Bool, VarValue::Bool(x)) => w.write_bool(*x),
            (VarType::Array { elem, count }, VarValue::Array(xs)) => {
                self.validate()?;
                if xs.len() != usize::from(*count) {
                    return Err(CodecError::Layout);
                }
                xs.iter().try_for_each(|x| elem.write(x, w))
            }
            (VarType::WString { max }, VarValue::WString(s)) => {
                if s.len() > usize::from(*max) {
                    return Err(CodecError::TooLong {
                        len: s.len(),
                        max: usize::from(*max),
                    });
                }
                w.write(s.len() as u32, Self::len_bits(*max))?;
                s.iter().try_for_each(|c| w.write(u32::from(*c), 16))
            }
            (VarType::CString { max }, VarValue::CString(s)) => {
                if s.len() > usize::from(*max) {
                    return Err(CodecError::TooLong {
                        len: s.len(),
                        max: usize::from(*max),
                    });
                }
                w.write(s.len() as u32, Self::len_bits(*max))?;
                s.iter().try_for_each(|c| w.write(u32::from(*c), 8))
            }
            _ => Err(CodecError::Layout),
        }
    }
}

/// One object type's variable list (§7): `vars[n]` is variable id `n`, in registration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectSchema {
    pub name: &'static str,
    /// The constructor's capacity; the variable-id width is `bits(count)` (§4.5, §7). An object
    /// may register fewer variables than its count.
    pub count: u32,
    pub vars: Vec<(&'static str, VarType)>,
}

impl ObjectSchema {
    /// The variable-id width: the bit length of the variable count (§4.5).
    pub fn var_id_bits(&self) -> u8 {
        bit_length(self.count)
    }

    /// Decodes a payload's `(variable id, value)` pairs; they must fill exactly the payload.
    pub fn decode(&self, payload: &BitRun) -> Result<Vec<(u32, VarValue)>, CodecError> {
        let idb = self.var_id_bits();
        if idb == 0 {
            return if payload.is_empty() {
                Ok(Vec::new())
            } else {
                Err(CodecError::Payload)
            };
        }
        let mut r = payload.reader();
        let mut out = Vec::new();
        while r.remaining() > 0 {
            let id = r.read(idb).map_err(|_| CodecError::Payload)?;
            let (_, t) = self.vars.get(id as usize).ok_or(CodecError::Payload)?;
            let v = t.read(&mut r).map_err(|e| match e {
                CodecError::Truncated => CodecError::Payload,
                e => e,
            })?;
            out.push((id, v));
        }
        Ok(out)
    }

    /// Encodes `(variable id, value)` pairs into a payload (at most `2^14 - 1` bits).
    pub fn encode(&self, vars: &[(u32, VarValue)]) -> Result<BitRun, CodecError> {
        let idb = self.var_id_bits();
        let mut w = BitWriter::new();
        for (id, v) in vars {
            let (_, t) = self.vars.get(*id as usize).ok_or(CodecError::Layout)?;
            w.write(*id, idb)?;
            t.write(v, &mut w)?;
        }
        if w.len() > super::object::PAYLOAD_MAX_BITS {
            return Err(CodecError::Range);
        }
        Ok(w.into_run())
    }
}

/// `SessionHostSynced`, the Lobby router's (6) type 0 (§7.1): count 15, 13 registered.
pub fn session_host_synced() -> ObjectSchema {
    use VarType::{Bool, CString, F32, SInt, UInt};
    ObjectSchema {
        name: "SessionHostSynced",
        count: 15,
        vars: vec![
            ("LobbyType", UInt { bits: 3 }),
            ("PrivacyMode", UInt { bits: 2 }),
            ("CountdownType", UInt { bits: 2 }),
            ("CountdownEndTime", UInt { bits: 32 }),
            ("RaceReady", Bool),
            ("RaceParticipantsSelected", Bool),
            ("InRace", Bool),
            ("LastInactivePlayersPulledTime", UInt { bits: 32 }),
            ("RoundGuid", CString { max: 36 }),
            ("RaceStartTime", UInt { bits: 32 }),
            ("NumLapsCompleted", UInt { bits: 8 }),
            ("FurthestProgress", F32),
            ("GroupSatNavNode", SInt { bits: 32 }),
        ],
    }
}

/// `ConsoleSynced`, the ConsoleManager router's (3) only type (§7.2): count 9.
pub fn console_synced() -> ObjectSchema {
    use VarType::{Bool, UInt};
    ObjectSchema {
        name: "ConsoleSynced",
        count: 9,
        vars: vec![
            ("RenderReady", Bool),
            ("SessionEntryFinalised", Bool),
            ("ParticipatingInRace", Bool),
            ("RaceInProgress", Bool),
            ("Ready", Bool),
            ("HasLoadedCarsForAllParticipants", Bool),
            ("HasGrid", Bool),
            ("AvailableArbitration", UInt { bits: 2 }),
            ("MatchTicketArbiter", UInt { bits: 2 }),
        ],
    }
}

fn u(bits: u8) -> VarType {
    VarType::UInt { bits }
}

fn arr(elem: VarType, count: u16) -> VarType {
    VarType::Array {
        elem: Box::new(elem),
        count,
    }
}

const S32: VarType = VarType::SInt { bits: 32 };
const F: VarType = VarType::F32;
const B: VarType = VarType::Bool;

/// `PlayerData`, the Player router's (1) object (§7.3): count 163, ids 0-161 (its own members,
/// `LobbyPlayer`, `PlayerChoices` with `CarInitSynced`, then `SyncedRaceData`). Small ranges
/// with a negative minimum (`LocalSyncedId`, 7 bits) are kept as raw unsigned bits.
pub fn player_data() -> ObjectSchema {
    let mut v: Vec<(&'static str, VarType)> = vec![
        ("BufChoices", arr(u(8), 8)),
        ("HasLivery", B),
        ("IsNewLivery", B),
        ("NetworkName", VarType::WString { max: 16 }),
        ("XUID", VarType::U64),
        ("IsLeader", B),
        ("NumReceivedKickVotes", u(32)),
        ("HasFinalScore", B),
        ("HeatsRacePoints", u(32)),
        ("HeatsTotalPoints", u(32)),
        ("SeriesRacePoints", u(32)),
        ("SeriesTotalPoints", u(32)),
        // LobbyPlayer
        ("Ready", B),
        ("PlayerHost", B),
        ("IsPeerSpectator", B),
        ("PendingJoin", B),
        ("InitialCarLoading", B),
        ("HeadsetConnected", B),
        ("Invited", B),
        ("PlayerStatus", u(4)),
        ("PlayerType", u(3)),
        ("Points", u(16)),
        ("LastRacePoints", u(16)),
        ("AIPlayerId", u(32)),
        ("TrueSkillRating", u(32)),
        ("TransferProgress", F),
        ("Place", S32),
        ("XP", S32),
        ("XPOnSessionJoin", S32),
        ("VoiceChannel", B),
        ("LocalSyncedId", u(7)),
        ("PlayerFlags", u(8)),
        ("HasInitialCarBeenChosen", B),
        ("MissingTrackContent", B),
        ("HasGhostKingPerk", B),
        ("HasImmumeSystemPerk", B),
        ("HasBribeRidePerk", B),
        ("ClubID", S32),
        ("ClubRole", u(2)),
        ("ClubTag", VarType::WString { max: 4 }),
        ("PrivateUniverseId", u(5)),
    ];
    for _ in 0..12 {
        v.push(("ChatStates.Xuid", VarType::U64));
        v.push(("ChatStates.Muted", B));
        v.push(("ChatStates.Friend", B));
    }
    v.extend([
        ("Vote", u(3)),
        ("PlayerLevel", u(17)),
        ("PlayerTitleId", u(16)),
        ("PlayerBadgeId", u(16)),
        // PlayerChoices
        ("CarClass", S32),
        ("PerfIndex", F),
        ("SpeedRating", F),
        ("AccelRating", F),
        ("LaunchRating", F),
        ("BrakingRating", F),
        ("HandlingRating", F),
        ("AIRank", S32),
        ("AIIsFriend", B),
        ("TeamId", u(1)),
        ("PlayerBucketId", u(3)),
        ("PittingProfile", S32),
        ("LiveryId", VarType::U64),
        ("TuneId", VarType::U64),
        ("AssistSuggestedLine", u(2)),
        ("AssistBraking", u(2)),
        ("AssistSteering", u(2)),
        ("AssistStabilityControl", u(1)),
        ("AssistTractionControl", u(1)),
        ("AssistShifting", u(2)),
        ("AssistRewind", u(1)),
        ("AIDifficulty", u(3)),
        // CarInitSynced
        ("ModelId", S32),
        ("SharedId", S32),
        ("TireBrand", S32),
        ("WheelStyle", S32),
        ("Parts", arr(S32, 43)),
    ]);
    for name in [
        "FrontDownforce",
        "RearDownforce",
        "FinalDriveRatio",
        "BrakePressure",
        "BrakeBias",
        "HandbrakePressure",
        "CenterDiffTorqueSplit",
        "STMScale",
        "TCSTakeOffSlipDef",
        "TCSMovingSlipDef",
        "SteerSpeed",
        "SteerAmount",
        "FuelLoad",
    ] {
        v.push((name, F));
    }
    v.push(("GearRatios", arr(F, 9)));
    for _ in 0..2 {
        for name in [
            "Pressure",
            "Camber",
            "Toe",
            "Caster",
            "Springs",
            "Swaybar",
            "RideHeight",
            "DampBump",
            "DampRebound",
            "LSDTorqueAccel",
            "LSDTorqueDecel",
        ] {
            v.push((name, F));
        }
    }
    v.extend([
        ("UseStockSetup", B),
        ("ManufacturerColorIndex", u(32)),
        ("CarColorRGB", S32),
        ("CarColorSecondRGB", S32),
        ("MaterialIndex", u(32)),
        ("ForceOffRaceCarSettings", B),
        ("AutoBrakeOn", B),
        ("ABSOn", B),
        ("TCSOn", B),
        ("STMOn", B),
        ("SteeringType", u(2)),
        ("TransmissionType", u(2)),
        ("DamageDiff", u(2)),
        ("FuelTireWearDiff", u(2)),
        // SyncedRaceData
        ("NumDriftSkills", S32),
        ("NumEDriftSkills", S32),
        ("NumDraftSkills", S32),
        ("NumSkills", S32),
    ]);
    ObjectSchema {
        name: "PlayerData",
        count: 163,
        vars: v,
    }
}

/// `CarNetworked`, the Car router's (2) object (§7.4): 21 damage floats, then `CarUniverseId`.
pub fn car_networked() -> ObjectSchema {
    let names = [
        "CurRearDownforceDamage",
        "CurBodyDragDamage",
        "CurDrivelineDamage",
        "CurEngineDamage",
        "CurFrontDownforceDamage",
        "CurTransmissionDamage",
        "CurSteeringDamage",
        "CurClutchDamage",
        "Fuel",
        "CurWheel0Wear",
        "CurWheel0BrakeDamage",
        "CurWheel0SuspensionDmg",
        "CurWheel1Wear",
        "CurWheel1BrakeDamage",
        "CurWheel1SuspensionDmg",
        "CurWheel2Wear",
        "CurWheel2BrakeDamage",
        "CurWheel2SuspensionDmg",
        "CurWheel3Wear",
        "CurWheel3BrakeDamage",
        "CurWheel3SuspensionDmg",
    ];
    let mut vars: Vec<(&'static str, VarType)> = names.iter().map(|n| (*n, F)).collect();
    vars.push(("CarUniverseId", u(5)));
    ObjectSchema {
        name: "CarNetworked",
        count: 22,
        vars,
    }
}

/// `WeatherSynced`, the Weather router's (10) object (§7.5): count 11, 9 registered.
pub fn weather_synced() -> ObjectSchema {
    ObjectSchema {
        name: "WeatherSynced",
        count: 11,
        vars: vec![
            ("Atmosphere.CurrentIntensity", F),
            ("Atmosphere.CurrentTick", u(32)),
            ("Atmosphere.DesiredIntensity", F),
            ("Atmosphere.DesiredTick", u(32)),
            ("Atmosphere.DesiredPresetIndex", arr(S32, 4)),
            ("Precipitation.CurrentIntensity", F),
            ("Precipitation.CurrentTick", u(32)),
            ("Precipitation.DesiredIntensity", F),
            ("Precipitation.DesiredTick", u(32)),
        ],
    }
}

/// `CarMeetSynced`, the CarMeet router's (11) object (§7.6): count 18.
pub fn car_meet_synced() -> ObjectSchema {
    let mut vars: Vec<(&'static str, VarType)> = vec![
        ("GameModeInviteType", u(2)),
        ("InviteCountDownTime", u(32)),
        ("JoinEndTimeWagerRace", u(32)),
        ("JoinEndTimeGameMode", u(32)),
    ];
    for _ in 0..12 {
        vars.push(("JoinTypes.JoinType", u(2)));
    }
    vars.push(("GameModeSessionName", VarType::WString { max: 64 }));
    vars.push(("GameModeSessionTemplateName", VarType::WString { max: 64 }));
    ObjectSchema {
        name: "CarMeetSynced",
        count: 18,
        vars,
    }
}

/// `RoadTripSynced`, the RoadTrip router's (9) type 0 (§7.7): count 14, 10 registered.
pub fn road_trip_synced() -> ObjectSchema {
    ObjectSchema {
        name: "RoadTripSynced",
        count: 14,
        vars: vec![
            ("CurrentLocationId", S32),
            ("CompletedEventCount", u(3)),
            ("EventRaceId", S32),
            ("NextRaceId", S32),
            ("MinimumDriveToDestinationReward", u(14)),
            ("MaximumDriveToDestinationReward", u(14)),
            ("MaximumDriveToDestinationRewardEarntBefore", u(32)),
            ("DecreaseXPEnabled", B),
            ("State", u(4)),
            ("StateEndTime", u(32)),
        ],
    }
}

/// `RoadTripEventSelectionSynced`, the RoadTrip router's (9) type 1 (§7.8): count 93, 61
/// registered: the three location ids, then six `RoadTripLocationEvents` blocks of ten.
pub fn road_trip_event_selection() -> ObjectSchema {
    let mut vars: Vec<(&'static str, VarType)> = vec![("LocationIds", arr(S32, 3))];
    for _ in 0..6 {
        for _ in 0..4 {
            vars.push(("Events.RaceId", S32));
            vars.push(("Events.MinimumPlayers", u(4)));
        }
        vars.push(("CarClass", u(3)));
        vars.push(("VoteCount", u(5)));
    }
    ObjectSchema {
        name: "RoadTripEventSelectionSynced",
        count: 93,
        vars,
    }
}

/// `RoadTripPlayerSynced`, the RoadTrip router's (9) type 2 (§7.9): count 4, 3 registered.
pub fn road_trip_player() -> ObjectSchema {
    ObjectSchema {
        name: "RoadTripPlayerSynced",
        count: 4,
        vars: vec![
            ("State", u(4)),
            ("CompletedEventCount", u(3)),
            ("XPOnDestinationJoin", S32),
        ],
    }
}

/// `BucketListPlayerStateSynced`, the BucketList router's (12) type 0 (§7.10): count 6.
pub fn bucket_list_player_state() -> ObjectSchema {
    ObjectSchema {
        name: "BucketListPlayerStateSynced",
        count: 6,
        vars: vec![
            ("Score", F),
            ("CurrentSlotId", S32),
            ("CurrentChallengeId", S32),
            ("RequestingPartner", B),
            ("State", u(4)),
            ("ArrivedAtDestination", B),
        ],
    }
}

/// `BucketListSlotArraySynced`, the BucketList router's (12) type 1 (§7.11): count 61, twelve
/// slots of (SlotId, ChallengeId, OwnerSyncId).
pub fn bucket_list_slot_array() -> ObjectSchema {
    let mut vars = Vec::new();
    for _ in 0..12 {
        vars.push(("Slots.SlotId", S32));
        vars.push(("Slots.ChallengeId", S32));
        vars.push(("Slots.OwnerSyncId", S32));
    }
    ObjectSchema {
        name: "BucketListSlotArraySynced",
        count: 61,
        vars,
    }
}

/// `GameOptionsSynced`, the Lobby router's (6) type 1 (§7.12): count 670, ids 0-492.
pub fn game_options_synced() -> ObjectSchema {
    let mut v: Vec<(&'static str, VarType)> = vec![
        ("GameTypePreset", u(8)),
        ("EnvironmentFilter", u(8)),
        ("TrackFilterType", u(3)),
        ("UiRacificationMode", u(4)),
        ("TrackId", u(32)),
        ("RouteId", u(32)),
        ("TimeOfDayIndex", u(8)),
        ("TimeOfDay", S32),
        ("TargetTimeOfDay", S32),
        ("CollObjLayoutId", u(8)),
        ("CrowdSize", F),
        ("CrowdExcitement", F),
        ("MaxPlayers", u(4)),
        ("MaxAIPlayers", u(4)),
        ("NumPlayerBuckets", u(8)),
        ("e321Type", u(3)),
        ("DamageDifficulty", u(2)),
        ("CollisionMode", u(3)),
        ("SurfaceSet", u(1)),
        ("AIDifficulty", u(3)),
        // SeriesOptions
        ("NumSeriesRaces", u(8)),
        ("NumHeats", u(8)),
    ];
    for _ in 0..8 {
        v.extend([
            ("Bucket.Name", VarType::WString { max: 24 }),
            ("Bucket.RollOffDelay", F),
            ("Bucket.CanStartEndOfRaceCountdown", B),
            ("Bucket.MinPlayers", u(8)),
            ("Bucket.MaxPlayers", u(8)),
            ("Bucket.GameTorqueScale", F),
            ("Bucket.FrictionFrontScale", F),
            ("Bucket.FrictionRearScale", F),
            ("Bucket.CarClassId", u(8)),
            ("Bucket.PIMin", u(16)),
            ("Bucket.PIMax", u(16)),
            ("Bucket.PowerMin", F),
            ("Bucket.PowerMax", F),
            ("Bucket.WeightMin", F),
            ("Bucket.WeightMax", F),
            ("Bucket.YearMin", u(32)),
            ("Bucket.YearMax", u(32)),
        ]);
        // thirteen (comparator, id) pairs: body family, car type, region, country, make, model
        // family, model, drive type, engine config, engine placement, cylinder, aspiration,
        // family special
        for id_bits in [8, 8, 8, 8, 16, 16, 16, 8, 8, 8, 8, 8, 8] {
            v.push(("Bucket.Comparator", u(2)));
            v.push(("Bucket.Id", u(id_bits)));
        }
        v.extend([
            ("Bucket.AllowUpgrades", B),
            ("Bucket.MiniCareerId", S32),
            ("Bucket.Blacklist", arr(u(16), 50)),
            ("Bucket.Whitelist", arr(u(16), 50)),
        ]);
    }
    v.extend([
        ("TeamOptions.RollOffDelay", F),
        ("TeamOptions.RollOffDelay", F),
        // StartingGridOptions
        ("StartType", u(2)),
        ("GridOrderingType", u(3)),
        ("Ascending", B),
        ("Distribute", B),
        ("GroupByBucket", B),
        ("InterleaveTeams", B),
        ("CarsPerUniverse", u(8)),
        ("ClusterHumansAtIndex", VarType::SInt { bits: 8 }),
        ("FirstCarHeadStartMeters", F),
        ("PerCarRollOffDelay", F),
        ("SoccerKickoffRollOffDelay", F),
        // FieldWindowOptions
        ("FieldWindow.Enabled", B),
        ("FieldWindow.BucketId", u(8)),
        ("FieldWindow.ForwardCarCount", u(8)),
        ("FieldWindow.ReverseCarCount", u(8)),
        ("FieldWindow.TargetDistance", F),
        ("FieldWindow.ForwardSafeDistance", F),
        ("FieldWindow.ReverseSafeDistance", F),
        // EndConditionOptions
        ("EndCondition.Type", u(3)),
        ("EndCondition.NumLaps", u(32)),
        ("EndCondition.Time", F),
        ("EndCondition.DestinationBucket", u(8)),
        ("EndCondition.MatchLength", F),
        ("EndCondition.EndRaceCounter", F),
        // ScoringOptions
        ("Scoring.Type", u(4)),
        ("Scoring.BiggerIsBetter", B),
        ("Scoring.CompareBestLaps", B),
        ("Scoring.CompareFullRaceDirty", B),
        ("Scoring.NumTeams", u(2)),
        ("Scoring.TeamScoringType", u(3)),
        ("Scoring.LastTarget", u(8)),
        ("Scoring.TargetScoreEnabled", B),
        ("Scoring.TargetScoreClean", B),
        ("Scoring.ScoreBucketsIndependently", B),
        ("Scoring.FinishingPlaceRelevant", B),
        ("Scoring.TargetBucket", u(8)),
        ("Scoring.TargetScore", arr(F, 3)),
        ("Scoring.ForceFailedTarget", B),
        // TagOptions
        ("Tag.TagEnabled", B),
        ("Tag.NoTagBacksTime", F),
        ("Tag.GetAwayTime", F),
        ("Tag.TagItBucket", u(8)),
        ("Tag.ItBucketBecomes", u(8)),
        ("Tag.NotItBucketBecomes", u(8)),
        ("Tag.TwoNotItsBecome", u(8)),
        ("Tag.NumPlayersPerIt", u(8)),
        // Overrides
        ("ForceTeamColors", B),
        ("ForceStockUpgradesAndTuning", B),
        ("ForceOffSuggestedLine", B),
        ("ForceOffSTM", B),
        ("ForceOffTCS", B),
        ("EasiestBrakingAllowed", u(2)),
        ("EasiestSteeringAllowed", u(2)),
        ("EasiestShiftingAllowed", u(2)),
        ("ForcedCamera", u(3)),
        // HudOptions
        ("ForceOffTimers", B),
        ("ForceOffMiniScoreboard", B),
        ("ForceOffSplits", B),
        ("ForceOffGaugeCluster", B),
        ("ForceOffHeatAndDamage", B),
        ("ForceOffMinimap", B),
        ("DisableWrongWayIndicator", B),
        ("ForceOffPlace", B),
        ("ForceShowLabels", B),
        ("ForceGhostVisibility", u(3)),
        // LobbyLaunchOptions
        ("HopperId", S32),
        ("HopperSettingsIndex", u(16)),
        ("HopperSettingsOffset", u(16)),
        ("MinPlayersToLaunch", u(32)),
        ("IntermissionTimeMs", u(32)),
        ("VotingTimeMs", u(32)),
        ("LaunchTimeMs", u(32)),
        ("AllowVoteToSkip", B),
        ("AllowVoteToKick", B),
        ("AllowVoteToRebalance", B),
        ("HideTrueSkill", B),
        ("PayoutModifier", F),
        ("MaxPartySize", u(4)),
        // DirectWeatherControlSettings
        ("ControlWeather", B),
        ("ControlAtmosphericPreset", B),
        ("AtmosphericPresetIndex", S32),
        ("DesiredAtmosphericWetness", F),
        ("DesiredEnvironmentalWetness", F),
        ("DesiredPrecipitationLevel", F),
        ("PauseWeather", B),
        // own members, last block
        ("DrivatarTrainingDisabled", B),
        ("MaxDrivatars", u(4)),
        ("DraftingEnabled", B),
        ("UseCollisionSubobjects", B),
        ("HasTraffic", B),
        ("PositionCPFlaresFromCentre", B),
        ("UsePFXCheckpoints", B),
        ("UseOnlineAntiGriefing", B),
    ]);
    ObjectSchema {
        name: "GameOptionsSynced",
        count: 670,
        vars: v,
    }
}

/// The variable list of `(router, type)` (§7). An update does not name its type: the caller
/// takes it from the object's create.
pub fn object_schema(router: u8, type_index: u32) -> Option<ObjectSchema> {
    match (router, type_index) {
        (1, 0) => Some(player_data()),
        (2, 0) => Some(car_networked()),
        (3, 0) => Some(console_synced()),
        // SPEC: §7 says "Lobby (6)" for SessionHostSynced and "the second type" for
        // GameOptionsSynced, so SessionHostSynced is type 0 and GameOptionsSynced type 1.
        (6, 0) => Some(session_host_synced()),
        (6, 1) => Some(game_options_synced()),
        (9, 0) => Some(road_trip_synced()),
        (9, 1) => Some(road_trip_event_selection()),
        (9, 2) => Some(road_trip_player()),
        (10, 0) => Some(weather_synced()),
        (11, 0) => Some(car_meet_synced()),
        (12, 0) => Some(bucket_list_player_state()),
        (12, 1) => Some(bucket_list_slot_array()),
        _ => None,
    }
}
