// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Globalization;
using Microsoft.Data.Sqlite;

namespace Infinity.Core.WorldServer;

/// <summary>The upgrade rows one car model can carry, by slot, as this player's own game database lists them.</summary>
internal sealed class ModelParts(uint model, IReadOnlySet<uint>[] bySlot)
{
    internal uint Model { get; } = model;

    /// <summary>The rows of each of the <see cref="PartsCatalog.Slots"/> slots' tables that belong to this model.</summary>
    internal IReadOnlySet<uint>[] BySlot { get; } = bySlot;

    /// <summary>Every row of any slot's table that belongs to this model.</summary>
    internal IReadOnlySet<uint> All { get; } = bySlot.SelectMany(s => s).ToHashSet();
}

/// <summary>
/// NR-6 (MATCHMAKING_API §3.9 <c>parts</c>): the game's upgrade tables, read at runtime from the player's own installed game database
/// (gamedbRC.slt, read-only; nothing is copied or shipped). A part row belongs to a model when its table's scope column names the
/// model itself (<c>Ordinal</c>), or one of the engines, drivetrains, car bodies or motors the model's own rows offer (an engine swap
/// brings its engine's parts). Missing tables or columns, or a database that cannot be read: no catalog (nothing is measured).
/// </summary>
internal sealed class PartsCatalog
{
    /// <summary>The installed-upgrade slots of a car (the player object's part rows, SAURON V9 var 107).</summary>
    internal const int Slots = 43;
    internal const int MaxCachedModels = 64;

    internal enum Scope { Car, Engine, Drivetrain, Body, Motor }

    /// <summary>
    /// The slot tables in the order of the game's own garage columns (Engine .. MotorParts), with the column naming their scope.
    /// The exe's PartEnum table (SAURON) has the same groups: 0-3 Engine, Drivetrain, CarBody, Motor; 4-11 as here; 12-29 engine
    /// parts, 30-33 drivetrain, 34-41 body, 42 MotorParts. The order inside the engine, drivetrain and body groups is the garage's,
    /// not yet confirmed live; only the slot check depends on it, and it logs only.
    /// </summary>
    internal static readonly (string Table, Scope Scope, string Column)[] SlotTables =
    [
        ("List_UpgradeEngine", Scope.Car, "Ordinal"), ("List_UpgradeDrivetrain", Scope.Car, "Ordinal"),
        ("List_UpgradeCarBody", Scope.Car, "Ordinal"), ("List_UpgradeMotor", Scope.Car, "Ordinal"),
        ("List_UpgradeBrakes", Scope.Car, "Ordinal"), ("List_UpgradeSpringDamper", Scope.Car, "Ordinal"),
        ("List_UpgradeAntiSwayFront", Scope.Car, "Ordinal"), ("List_UpgradeAntiSwayRear", Scope.Car, "Ordinal"),
        ("List_UpgradeTireCompound", Scope.Car, "Ordinal"), ("List_UpgradeRearWing", Scope.Car, "Ordinal"),
        ("List_UpgradeRimSizeFront", Scope.Car, "Ordinal"), ("List_UpgradeRimSizeRear", Scope.Car, "Ordinal"),
        ("List_UpgradeEngineCamshaft", Scope.Engine, "EngineID"), ("List_UpgradeEngineValves", Scope.Engine, "EngineID"),
        ("List_UpgradeEngineDisplacement", Scope.Engine, "EngineID"), ("List_UpgradeEnginePistonsCompression", Scope.Engine, "EngineID"),
        ("List_UpgradeEngineFuelSystem", Scope.Engine, "EngineID"), ("List_UpgradeEngineIgnition", Scope.Engine, "EngineID"),
        ("List_UpgradeEngineExhaust", Scope.Engine, "EngineID"), ("List_UpgradeEngineIntake", Scope.Engine, "EngineID"),
        ("List_UpgradeEngineFlywheel", Scope.Engine, "EngineID"), ("List_UpgradeEngineManifold", Scope.Engine, "EngineID"),
        ("List_UpgradeEngineRestrictorPlate", Scope.Engine, "EngineID"), ("List_UpgradeEngineOilCooling", Scope.Engine, "EngineID"),
        ("List_UpgradeEngineTurboSingle", Scope.Engine, "EngineID"), ("List_UpgradeEngineTurboTwin", Scope.Engine, "EngineID"),
        ("List_UpgradeEngineTurboQuad", Scope.Engine, "EngineID"), ("List_UpgradeEngineCSC", Scope.Engine, "EngineID"),
        ("List_UpgradeEngineDSC", Scope.Engine, "EngineID"), ("List_UpgradeEngineIntercooler", Scope.Engine, "EngineID"),
        ("List_UpgradeDrivetrainClutch", Scope.Drivetrain, "DrivetrainID"), ("List_UpgradeDrivetrainTransmission", Scope.Drivetrain, "DrivetrainID"),
        ("List_UpgradeDrivetrainDriveline", Scope.Drivetrain, "DrivetrainID"), ("List_UpgradeDrivetrainDifferential", Scope.Drivetrain, "DrivetrainID"),
        ("List_UpgradeCarBodyFrontBumper", Scope.Body, "CarBodyID"), ("List_UpgradeCarBodyRearBumper", Scope.Body, "CarBodyID"),
        ("List_UpgradeCarBodyHood", Scope.Body, "CarBodyID"), ("List_UpgradeCarBodySideSkirt", Scope.Body, "CarBodyID"),
        ("List_UpgradeCarBodyTireWidthFront", Scope.Body, "CarBodyID"), ("List_UpgradeCarBodyTireWidthRear", Scope.Body, "CarBodyID"),
        ("List_UpgradeCarBodyWeight", Scope.Body, "CarBodyID"), ("List_UpgradeCarBodyChassisStiffness", Scope.Body, "CarBodyID"),
        ("List_UpgradeMotorParts", Scope.Motor, "MotorID"),
    ];

    /// <summary>Slots whose rows name the engine, drivetrain, body or motor every car has one of (the layout check).</summary>
    internal const int EngineSlot = 0, DrivetrainSlot = 1, BodySlot = 2, MotorSlot = 3;

    /// <summary>The model's own rows that name the scope ids of the other tables: (table, its scope column).</summary>
    internal static (string Table, string Column) Source(Scope scope) => scope switch
    {
        Scope.Engine => ("List_UpgradeEngine", "EngineID"),
        Scope.Drivetrain => ("List_UpgradeDrivetrain", "DrivetrainID"),
        Scope.Body => ("List_UpgradeCarBody", "CarBodyID"),
        Scope.Motor => ("List_UpgradeMotor", "MotorID"),
        _ => throw new ArgumentOutOfRangeException(nameof(scope)),
    };

    private readonly string _connection;
    private readonly string _query;
    private readonly Action<string> _log;
    private readonly Dictionary<uint, ModelParts?> _models = [];
    private readonly Lock _gate = new();
    private bool _broken;

    private PartsCatalog(string connection, Action<string> log)
    {
        _connection = connection;
        _log = log;
        // one statement per model: every slot's rows of this model (the ordinal itself, or the scope ids its own rows offer)
        _query = string.Join(" UNION ALL ", SlotTables.Select((slot, index) => slot.Scope == Scope.Car
            ? string.Create(CultureInfo.InvariantCulture, $"SELECT {index}, Id FROM [{slot.Table}] WHERE [{slot.Column}] = $model")
            : string.Create(CultureInfo.InvariantCulture,
                $"SELECT {index}, Id FROM [{slot.Table}] WHERE [{slot.Column}] IN (SELECT [{Source(slot.Scope).Column}] FROM [{Source(slot.Scope).Table}] WHERE Ordinal = $model)")));
    }

    /// <summary>
    /// The catalog of <paramref name="gameDatabase"/> (gamedbRC.slt, read-only), or null with one log line when it cannot be read or
    /// lacks a table or column this check needs (then nothing is measured).
    /// </summary>
    internal static PartsCatalog? Load(string? gameDatabase, Action<string>? log = null)
    {
        var say = log ?? (_ => { });
        if (string.IsNullOrEmpty(gameDatabase) || !File.Exists(gameDatabase))
        {
            say("parts: no game database on this PC; the parts rule measures nothing");
            return null;
        }
        var connection = new SqliteConnectionStringBuilder { DataSource = gameDatabase, Mode = SqliteOpenMode.ReadOnly, Pooling = false }.ToString();
        try
        {
            using var db = new SqliteConnection(connection);
            db.Open();
            Scope[] scopes = [Scope.Engine, Scope.Drivetrain, Scope.Body, Scope.Motor];
            (string Table, string Column)[] needed =
            [
                ("Data_Car", "Id"),
                .. SlotTables.Select(s => (s.Table, s.Column)), .. SlotTables.Select(s => (s.Table, "Id")),
                .. scopes.Select(s => (Source(s).Table, "Ordinal")), .. scopes.Select(Source),
            ];
            foreach (var group in needed.GroupBy(n => n.Table))
            {
                var columns = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
                using var command = db.CreateCommand();
                command.CommandText = $"SELECT name FROM pragma_table_info('{group.Key}')";
                using (var rows = command.ExecuteReader()) while (rows.Read()) columns.Add(rows.GetString(0));
                if (group.FirstOrDefault(n => !columns.Contains(n.Column)) is { Table: not null } missing)
                {
                    say(columns.Count == 0 ? $"parts: the game database has no table {missing.Table}; the parts rule measures nothing"
                        : $"parts: the game database's {missing.Table} has no column {missing.Column}; the parts rule measures nothing");
                    return null;
                }
            }
            return new(connection, say);
        }
        catch (Exception error) when (error is SqliteException or IOException or UnauthorizedAccessException or InvalidOperationException)
        {
            say("parts: the game database cannot be read (" + error.GetType().Name + "); the parts rule measures nothing");
            return null;
        }
    }

    /// <summary>
    /// The rows of <paramref name="model"/>, or null when this database does not know the model (a car-pack car, a placeholder) or
    /// cannot be read any more (logged once; from then on every model is unknown).
    /// </summary>
    internal ModelParts? Model(uint model)
    {
        lock (_gate)
        {
            if (_broken) return null;
            if (_models.TryGetValue(model, out var known)) return known;
            if (_models.Count >= MaxCachedModels) _models.Clear();
            try
            {
                using var db = new SqliteConnection(_connection);
                db.Open();
                using (var exists = db.CreateCommand())
                {
                    exists.CommandText = "SELECT 1 FROM Data_Car WHERE Id = $model";
                    exists.Parameters.AddWithValue("$model", (long)model);
                    if (exists.ExecuteScalar() is null) return _models[model] = null;
                }
                var slots = Enumerable.Range(0, Slots).Select(_ => new HashSet<uint>()).ToArray();
                using var command = db.CreateCommand();
                command.CommandText = _query;
                command.Parameters.AddWithValue("$model", (long)model);
                using var rows = command.ExecuteReader();
                while (rows.Read())
                {
                    var id = rows.GetInt64(1);
                    if (id is > 0 and <= uint.MaxValue) slots[rows.GetInt32(0)].Add((uint)id);
                }
                return _models[model] = new ModelParts(model, slots);
            }
            catch (Exception error) when (error is SqliteException or IOException or UnauthorizedAccessException or InvalidOperationException)
            {
                _broken = true;
                _log("parts: the game database cannot be read any more (" + error.GetType().Name + "); the parts rule measures nothing");
                return null;
            }
        }
    }
}
