CREATE TABLE [AddOnState]   (
[AddOnId] INTEGER PRIMARY KEY 
,[IsPurchased] INT
);
CREATE TABLE [BarnFinds]   (
[CarOrdinal] INTEGER PRIMARY KEY 
,[State] INT
);
CREATE TABLE [Career_EventCarsClaimed]   (
[RewardsEventCarsId] INTEGER PRIMARY KEY 
);
CREATE TABLE [Career_Garage]   (
[Id] INTEGER PRIMARY KEY 
,[CarId] INT
,[PerformanceIndex] REAL
,[ClassID] INT
,[PartsValue] INT
,[SpeedRating] REAL
,[AccelerationRating] REAL
,[LaunchRating] REAL
,[BrakingRating] REAL
,[HandlingRating] REAL
,[CurbWeight] REAL
,[WeightDistribution] REAL
,[AspirationTypeId] INT
,[SimPeakPower] REAL
,[SimPeakAngVel] REAL
,[SimPeakTorque] REAL
,[SimPeakTorqueAngVel] REAL
,[SimRedlineAngVel] REAL
,[PeakIntakePSI] REAL
,[TopSpeed] REAL
,[DistanceDriven] INT
,[TimeDriven] INT
,[TotalWinnings] INT
,[TotalRepairs] INT
,[NumVictories] INT
,[NumPodiums] INT
,[NumRaces] INT
,[NumOwners] INT
,[NumTimesSold] INT
,[TimeDrivenInRoadTrips] INT
,[CurOwnerNumRaces] INT
,[CurOwnerWinnings] INT
,[NumSkillPointsEarned] INT
,[OriginalOwner] TEXT
,[CarGroup] INT
,[Flags] INT
,[Engine] INT
,[Drivetrain] INT
,[CarBody] INT
,[Motor] INT
,[Brakes] INT
,[SpringDamper] INT
,[AntiSwayFront] INT
,[AntiSwayRear] INT
,[TireCompound] INT
,[RearWing] INT
,[RimSizeFront] INT
,[RimSizeRear] INT
,[Camshaft] INT
,[Valves] INT
,[Displacement] INT
,[PistonsCompression] INT
,[FuelSystem] INT
,[Ignition] INT
,[Exhaust] INT
,[Intake] INT
,[Flywheel] INT
,[Manifold] INT
,[RestrictorPlate] INT
,[OilCooling] INT
,[SingleTurbo] INT
,[TwinTurbo] INT
,[QuadTurbo] INT
,[SuperchargerCSC] INT
,[SuperchargerDSC] INT
,[Intercooler] INT
,[Clutch] INT
,[Transmission] INT
,[Driveline] INT
,[Differential] INT
,[FrontBumper] INT
,[RearBumper] INT
,[Hood] INT
,[SideSkirts] INT
,[TireWidthFront] INT
,[TireWidthRear] INT
,[WeightReduction] INT
,[ChassisStiffness] INT
,[MotorParts] INT
,[TireBrand] INT
,[WheelStyle] INT
,[DefaultManufacturerColorIndex] INT
,[LiveryFileName] TEXT
,[TuneFileName] TEXT
,[VersionedTuneId] BIGINT
,[VersionedLiveryId] BIGINT
,[Guid] TEXT
,[Thumbnail] TEXT
,[Tuning_frontTirePressure] REAL
,[Tuning_rearTirePressure] REAL
,[Tuning_finalDriveRatio] REAL
,[Tuning_firstGear] REAL
,[Tuning_secondGear] REAL
,[Tuning_thirdGear] REAL
,[Tuning_fourthGear] REAL
,[Tuning_fifthGear] REAL
,[Tuning_sixthGear] REAL
,[Tuning_seventhGear] REAL
,[Tuning_eighthGear] REAL
,[Tuning_frontCamber] REAL
,[Tuning_rearCamber] REAL
,[Tuning_frontToe] REAL
,[Tuning_rearToe] REAL
,[Tuning_frontCaster] REAL
,[Tuning_frontSwaybar] REAL
,[Tuning_rearSwaybar] REAL
,[Tuning_frontSpring] REAL
,[Tuning_rearSpring] REAL
,[Tuning_frontRideHeight] REAL
,[Tuning_rearRideHeight] REAL
,[Tuning_frontDampingStiffness] REAL
,[Tuning_rearDampingStiffness] REAL
,[Tuning_frontBumpRatio] REAL
,[Tuning_rearBumpRatio] REAL
,[Tuning_frontDownforce] REAL
,[Tuning_rearDownforce] REAL
,[Tuning_brakeBalance] REAL
,[Tuning_brakePressure] REAL
,[Tuning_frontAccel] REAL
,[Tuning_rearAccel] REAL
,[Tuning_frontDecel] REAL
,[Tuning_rearDecel] REAL
,[Tuning_centerTorque] REAL
,[SharedID] INT
);
CREATE TABLE [Career_PurchasedParts]   (
[GarageId] INT
,[PartEnum] INT
,[PartId] INT
,[ManufacturerId] INT
,[PricePaid] INT
);
CREATE TABLE [FreeCars]   (
[CarId] INTEGER PRIMARY KEY 
,[FreeCount] INT
);
CREATE TABLE [PerkUnlock]   (
[UnlockId] INTEGER PRIMARY KEY 
);
