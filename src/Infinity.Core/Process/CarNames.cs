// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Globalization;
using System.IO.Compression;
using System.Text;
using System.Text.Json;
using Infinity.Core.Mods;
using Microsoft.Data.Sqlite;

namespace Infinity.Core.Process;

/// <summary>
/// CR-L41.3 / T10-T11 (docs/launcher/DISCORD_PRESENCE.md §2, re/car-name.md §3b): the hook's car key (current+0x40,
/// <c>Data_Car.Id × 1000</c>) as <c>"&lt;Year&gt; &lt;Make&gt; &lt;Model&gt;"</c>, rebuilt from files the way the game sees them:
/// <list type="number">
/// <item>the base database is <c>game:\media\Stripped\gamedbRC.slt</c> as Azimuth maps it, the first of
/// <c>Mount/{mediaoverride|mediapc|media}/stripped/gamedbRC.slt</c> (read-only SQLite); never a car pack's source copy;</item>
/// <item>every enabled ExtraOptions mod (<c>&lt;Azimuth&gt;/mods/loadorder.txt</c>, <see cref="ExtraOptionsMods.Scan"/>), first to
/// last, replays its <c>database.json</c> on an in-memory copy, as ExtraOptions does on its own (<c>features/addon_database.c</c>:
/// per table deletes, updates, inserts; one transaction per mod; the later mod wins);</item>
/// <item>a row a <c>policy.id_conflicts: "nearest"</c> mod touched gives no name: that mod may run under other ids, known only from
/// the session's log;</item>
/// <item><c>Year</c>, <c>List_CarMake.DisplayName</c> (by <c>MakeID</c>) and <c>Data_Car.DisplayName</c>; a <c>_&amp;&lt;digits&gt;</c>
/// value is string id <c>value &amp; 0xFFFF</c>, looked up in the enabled mods' <c>strings.json</c> last to first (the language, then
/// that mod's <c>fallback_language</c>, default <c>EN</c>; <c>features/addon_strings.c</c>), then in <c>&lt;Table&gt;.str</c> of the
/// first <c>Mount/{mediaoverride|mediapc|media}/stripped/stringtables/&lt;LANG&gt;.zip</c>; any other value is literal text.</item>
/// </list>
/// Anything missing is null. Names live only in this object's memory: never stored, logged or shipped. <see cref="Name"/> reads files
/// on its first calls: call it off the UI thread.
/// </summary>
public sealed class CarNames
{
    /// <summary>A string table larger than this is not read (the game's are far smaller).</summary>
    public const long MaxStringTableBytes = 16 * 1024 * 1024;

    /// <summary>A mod's <c>database.json</c> or <c>strings.json</c> larger than this is not read.</summary>
    public const long MaxModJsonBytes = 32 * 1024 * 1024;

    /// <summary>The game's media roots, highest rank first (Azimuth's overlays: mediaoverride, then mediapc, then media).</summary>
    public static readonly IReadOnlyList<string> MediaRoots = ["mediaoverride", "mediapc", "media"];

    private const string CarTable = "Data_Car", MakeTable = "List_CarMake";

    private readonly string _azimuthRoot;
    private readonly string _mount;
    private readonly string? _database;
    private readonly string _language;
    private readonly string[] _zips;
    private readonly Lock _gate = new();
    private readonly Dictionary<int, string?> _names = [];
    private readonly Dictionary<string, (Dictionary<ushort, string> Models, Dictionary<ushort, string> Makes)?> _tables =
        new(StringComparer.OrdinalIgnoreCase);
    private List<Mod>? _mods;
    private SqliteConnection? _merged;
    private bool _mergeTried;
    /// <summary>The mods ExtraOptions would keep (their <c>strings.json</c> count): no database, a replayed one, or a "nearest" one.</summary>
    private readonly List<Mod> _applied = [];
    /// <summary>(table, id) rows a "nearest" mod touches: no name for them.</summary>
    private readonly HashSet<(string Table, long Id)> _tainted = [];

    private sealed record Mod(string Folder, JsonDocument? Database, JsonDocument? Strings, bool Nearest);

    /// <param name="azimuthRoot">The Azimuth folder (its <c>mods/</c> holds ExtraOptions' mods and <c>loadorder.txt</c>).</param>
    /// <param name="mount">The game's Mount folder (the one holding <c>media</c>).</param>
    /// <param name="language">The launcher's language: <c>pt-BR</c> reads ExtraOptions' <c>BR</c> (<c>br.zip</c>, then <c>EN.zip</c> for a missing string); anything else <c>EN</c>.</param>
    /// <param name="database">Tests only: a base database in place of <see cref="ActiveFile"/>'s.</param>
    public CarNames(string azimuthRoot, string mount, string? language, string? database = null)
    {
        ArgumentException.ThrowIfNullOrEmpty(azimuthRoot);
        ArgumentException.ThrowIfNullOrEmpty(mount);
        _azimuthRoot = azimuthRoot;
        _mount = mount;
        _database = database;
        var portuguese = language is not null && language.StartsWith("pt", StringComparison.OrdinalIgnoreCase);
        _language = portuguese ? "BR" : "EN";
        _zips = portuguese ? ["br.zip", "EN.zip"] : ["EN.zip"];
    }

    /// <summary>
    /// The file the game reads for <paramref name="relative"/> (below the media root, e.g. <c>stripped/gamedbRC.slt</c>): the first of
    /// <see cref="MediaRoots"/> under <paramref name="mount"/> that has it, or null.
    /// </summary>
    public static string? ActiveFile(string mount, string relative)
    {
        foreach (var root in MediaRoots)
        {
            var path = Path.Combine(mount, root, relative);
            if (File.Exists(path)) return path;
        }
        return null;
    }

    /// <summary>The <c>Data_Car.Id</c> of a hook car key (<c>Id × 1000</c>), or null when it is not one.</summary>
    public static int? CarId(int carKey) => carKey > 0 && carKey % 1000 == 0 ? carKey / 1000 : null;

    /// <summary><c>"&lt;Year&gt; &lt;Make&gt; &lt;Model&gt;"</c> of the hook's <paramref name="carKey"/>, or null; cached for this object's life.</summary>
    public string? Name(int carKey)
    {
        if (CarId(carKey) is not { } id) return null;
        lock (_gate)
        {
            if (_names.TryGetValue(id, out var cached)) return cached;
            var name = Resolve(id);
            _names[id] = name;
            return name;
        }
    }

    /// <summary>Forgets every name, mod, merged database and table (a new game session, or the install changed).</summary>
    public void Clear()
    {
        lock (_gate)
        {
            _names.Clear();
            _tables.Clear();
            _merged?.Dispose();
            _merged = null;
            _mergeTried = false;
            _applied.Clear();
            _tainted.Clear();
            if (_mods is not null) foreach (var mod in _mods) { mod.Database?.Dispose(); mod.Strings?.Dispose(); }
            _mods = null;
        }
    }

    private string? Resolve(int id)
    {
        try
        {
            if (Merged() is not { } db || _tainted.Contains((CarTable, id))) return null;
            long year, makeId;
            object? model;
            using (var command = db.CreateCommand())
            {
                command.CommandText = "SELECT Year, MakeID, DisplayName FROM Data_Car WHERE Id = $id LIMIT 1";
                command.Parameters.AddWithValue("$id", id);
                using var reader = command.ExecuteReader();
                if (!reader.Read() || !TryInteger(reader.IsDBNull(0) ? null : reader.GetValue(0), out year) || year is < 1 or > 9999
                    || !TryInteger(reader.IsDBNull(1) ? null : reader.GetValue(1), out makeId)) return null;
                model = reader.IsDBNull(2) ? null : reader.GetValue(2);
            }
            if (_tainted.Contains((MakeTable, makeId))) return null;
            object? make;
            using (var command = db.CreateCommand())
            {
                command.CommandText = "SELECT DisplayName FROM List_CarMake WHERE ID = $id LIMIT 1";
                command.Parameters.AddWithValue("$id", makeId);
                make = command.ExecuteScalar();
            }
            if (Text(MakeTable, make is DBNull ? null : make) is not { } makeName || Text(CarTable, model) is not { } modelName) return null;
            return $"{year.ToString(CultureInfo.InvariantCulture)} {makeName} {modelName}";
        }
        catch (Exception error) when (error is SqliteException or IOException or UnauthorizedAccessException or InvalidDataException
            or InvalidCastException or FormatException or OverflowException or JsonException or InvalidOperationException)
        {
            return null;
        }
    }

    // ------------------------------------------------------------- database

    /// <summary>
    /// The database as the game gets it: an in-memory copy of the base file with every enabled mod's <c>database.json</c> replayed
    /// on it, first to last, one savepoint per mod (a mod that fails is rolled back, as ExtraOptions does). A "nearest" mod is not
    /// replayed: the <c>Data_Car</c> / <c>List_CarMake</c> ids it names are recorded instead (<see cref="_tainted"/>), since it may run
    /// under other ids. Built once per instance; null without a readable base file.
    /// </summary>
    private SqliteConnection? Merged()
    {
        if (_mergeTried) return _merged;
        _mergeTried = true;
        if ((_database ?? ActiveFile(_mount, Path.Combine("stripped", "gamedbRC.slt"))) is not { } path || !File.Exists(path)) return null;
        var memory = new SqliteConnection("Data Source=:memory:;Pooling=False");
        try
        {
            using (var source = new SqliteConnection(new SqliteConnectionStringBuilder
                   { DataSource = path, Mode = SqliteOpenMode.ReadOnly, Pooling = false }.ToString()))
            {
                source.Open();
                memory.Open();
                source.BackupDatabase(memory);
            }
            foreach (var mod in Mods())
            {
                if (mod.Database is null) { _applied.Add(mod); continue; }
                if (mod.Nearest) { Taint(mod.Database.RootElement); _applied.Add(mod); continue; }
                if (Replay(memory, mod.Database.RootElement)) _applied.Add(mod);
            }
            _merged = memory;
            return memory;
        }
        catch
        {
            memory.Dispose();
            throw;
        }
    }

    /// <summary>
    /// One <c>database.json</c> on <paramref name="db"/>, as <c>features/addon_database.c</c> runs it: per table its deletes, then its
    /// updates, then its inserts. Format 1 only inserts; format 2 needs each table's <c>key</c>, and an update or delete names exactly
    /// those columns in <c>where</c>, which must match one row. Without <c>policy.update_conflicts: "last_wins"</c> an update or delete
    /// needs an <c>expect</c> that matches that row too. Any failure, a SQLite error included (an id already taken, an unknown table or
    /// column), rolls the whole mod back. Not modelled: <c>requires.records</c> and foreign-key checks.
    /// </summary>
    private static bool Replay(SqliteConnection db, JsonElement root)
    {
        var version = Member(root, "format_version") is { ValueKind: JsonValueKind.Number } v && v.TryGetInt32(out var n) ? n : 0;
        var strict = !(Member(Member(root, "policy"), "update_conflicts") is { ValueKind: JsonValueKind.String } c && c.GetString() == "last_wins");
        Execute(db, "SAVEPOINT fi_car_names");
        try
        {
            foreach (var table in Member(root, "tables")!.Value.EnumerateObject())
            {
                if (table.Value.ValueKind != JsonValueKind.Object) throw new InvalidDataException("table");
                var keys = Member(table.Value, "key");
                if ((version == 2 && keys is null) || (keys is { } k && (k.ValueKind != JsonValueKind.Array || k.GetArrayLength() == 0
                    || k.EnumerateArray().Any(x => x.ValueKind != JsonValueKind.String || x.GetString() is not { Length: > 0 })))) throw new InvalidDataException("key");
                foreach (var operation in table.Value.EnumerateObject())
                    if (operation.Name is not ("key" or "insert" or "update" or "delete")) throw new InvalidDataException("operation");
                foreach (var kind in new[] { "delete", "update", "insert" })
                {
                    if (Member(table.Value, kind) is not { } rows) continue;
                    if (rows.ValueKind != JsonValueKind.Array || (version == 1 && kind != "insert")) throw new InvalidDataException("operation");
                    foreach (var row in rows.EnumerateArray())
                    {
                        if (kind == "insert") Insert(db, table.Name, row);
                        else Mutate(db, table.Name, row, keys, strict, kind == "delete");
                    }
                }
            }
            Execute(db, "RELEASE fi_car_names");
            return true;
        }
        catch (Exception error) when (error is SqliteException or InvalidDataException or FormatException or InvalidOperationException)
        {
            Execute(db, "ROLLBACK TO fi_car_names");
            Execute(db, "RELEASE fi_car_names");
            return false;
        }
    }

    private static void Insert(SqliteConnection db, string table, JsonElement row)
    {
        if (row.ValueKind != JsonValueKind.Object || !row.EnumerateObject().Any()) throw new InvalidDataException("row");
        using var command = db.CreateCommand();
        var fields = row.EnumerateObject().ToList();
        command.CommandText = $"INSERT INTO {Name(table)} ({string.Join(",", fields.Select(f => Name(f.Name)))}) VALUES ("
            + string.Join(",", fields.Select((f, i) => "$v" + i.ToString(CultureInfo.InvariantCulture))) + ")";
        for (var i = 0; i < fields.Count; i++) command.Parameters.AddWithValue("$v" + i.ToString(CultureInfo.InvariantCulture), Parameter(fields[i].Value));
        command.ExecuteNonQuery();
    }

    private static void Mutate(SqliteConnection db, string table, JsonElement operation, JsonElement? keys, bool strict, bool remove)
    {
        var where = Member(operation, "where");
        var expect = Member(operation, "expect");
        var set = Member(operation, "set");
        if (operation.ValueKind != JsonValueKind.Object || keys is not { } keyList || where is not { ValueKind: JsonValueKind.Object } w
            || w.EnumerateObject().Count() != keyList.GetArrayLength()
            || (!remove && (set is not { ValueKind: JsonValueKind.Object } s || !s.EnumerateObject().Any()))
            || (expect is { } e && (e.ValueKind != JsonValueKind.Object || !e.EnumerateObject().Any())) || (strict && expect is null))
            throw new InvalidDataException("operation");
        foreach (var key in keyList.EnumerateArray())
        {
            if (Member(w, key.GetString()!) is not { } value || value.ValueKind == JsonValueKind.Null || (!remove && Member(set, key.GetString()!) is not null))
                throw new InvalidDataException("where");
        }
        if (Count(db, table, w, null) != 1 || (strict && Count(db, table, w, expect) != 1)) throw new InvalidDataException("no single row");
        using var command = db.CreateCommand();
        var assignments = remove ? "" : " SET " + string.Join(",", set!.Value.EnumerateObject().Select((f, i) =>
        {
            command.Parameters.AddWithValue("$s" + i.ToString(CultureInfo.InvariantCulture), Parameter(f.Value));
            return Name(f.Name) + "=$s" + i.ToString(CultureInfo.InvariantCulture);
        }));
        command.CommandText = (remove ? "DELETE FROM " : "UPDATE ") + Name(table) + assignments + " WHERE " + Predicate(command, w, "w");
        command.ExecuteNonQuery();
    }

    private static long Count(SqliteConnection db, string table, JsonElement where, JsonElement? expect)
    {
        using var command = db.CreateCommand();
        var predicate = Predicate(command, where, "w") + (expect is { } e ? " AND " + Predicate(command, e, "e") : "");
        command.CommandText = "SELECT COUNT(*) FROM " + Name(table) + " WHERE " + predicate;
        return (long)command.ExecuteScalar()!;
    }

    private static string Predicate(SqliteCommand command, JsonElement fields, string prefix) =>
        string.Join(" AND ", fields.EnumerateObject().Select((f, i) =>
        {
            var name = "$" + prefix + i.ToString(CultureInfo.InvariantCulture);
            command.Parameters.AddWithValue(name, Parameter(f.Value));
            return Name(f.Name) + " IS " + name;
        }));

    private static object Parameter(JsonElement value) => value.ValueKind switch
    {
        JsonValueKind.String => value.GetString()!,
        JsonValueKind.Number => value.TryGetInt64(out var l) ? l : value.GetDouble(),
        JsonValueKind.True => 1L,
        JsonValueKind.False => 0L,
        JsonValueKind.Null => DBNull.Value,
        _ => throw new InvalidDataException("value"),
    };

    private static string Name(string identifier) => "\"" + identifier.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"";

    private static void Execute(SqliteConnection db, string sql)
    {
        using var command = db.CreateCommand();
        command.CommandText = sql;
        command.ExecuteNonQuery();
    }

    /// <summary>The <c>Data_Car</c> / <c>List_CarMake</c> ids a "nearest" mod inserts, updates or deletes.</summary>
    private void Taint(JsonElement root)
    {
        foreach (var table in new[] { CarTable, MakeTable })
        {
            if (Member(Member(root, "tables"), table) is not { ValueKind: JsonValueKind.Object } operations) continue;
            foreach (var kind in new[] { "insert", "update", "delete" })
            {
                if (Member(operations, kind) is not { ValueKind: JsonValueKind.Array } rows) continue;
                foreach (var row in rows.EnumerateArray())
                {
                    var holder = kind == "insert" ? row : Member(row, "where") ?? default;
                    if (Member(holder, "ID") is { ValueKind: JsonValueKind.Number } value && value.TryGetInt64(out var id)) _tainted.Add((table, id));
                }
            }
        }
    }

    private static bool TryInteger(object? value, out long result)
    {
        result = 0;
        switch (value)
        {
            case long l: result = l; return true;
            case int i: result = i; return true;
            case double d when d == Math.Floor(d) && d is >= long.MinValue and <= long.MaxValue: result = (long)d; return true;
            case string s: return long.TryParse(s, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out result);
            default: return false;
        }
    }

    /// <summary>An object member by name, ignoring case (SQLite names and ExtraOptions' string tables do).</summary>
    private static JsonElement? Member(JsonElement? element, string name)
    {
        if (element is not { ValueKind: JsonValueKind.Object } value) return null;
        foreach (var member in value.EnumerateObject())
            if (string.Equals(member.Name, name, StringComparison.OrdinalIgnoreCase)) return member.Value;
        return null;
    }

    // --------------------------------------------------------------- strings

    /// <summary>A name part: a <c>_&amp;&lt;digits&gt;</c> reference resolved, other text as it is; null when missing.</summary>
    private string? Text(string table, object? value)
    {
        if (value is not string text) return null;
        if (StringKey(text) is not { } key) return text.Trim() is { Length: > 0 } literal ? literal : null;
        for (var i = _applied.Count - 1; i >= 0; i--)
        {
            if (_applied[i].Strings is not { } strings) continue;
            var languages = Member(strings.RootElement, "languages");
            var fallback = Member(strings.RootElement, "fallback_language") is { ValueKind: JsonValueKind.String } f ? f.GetString()! : "EN";
            foreach (var language in new[] { _language, fallback })
                if (Member(Member(Member(languages, language), table), key.ToString(CultureInfo.InvariantCulture)) is { ValueKind: JsonValueKind.String } found)
                    return found.GetString()!.Trim() is { Length: > 0 } modText ? modText : null; // the mod's text replaces the native one
        }
        foreach (var zip in _zips)
        {
            if (Tables(zip) is not { } tables) continue;
            if ((table == CarTable ? tables.Models : tables.Makes).TryGetValue(key, out var native) && native.Trim() is { Length: > 0 } nativeText)
                return nativeText;
        }
        return null;
    }

    private (Dictionary<ushort, string> Models, Dictionary<ushort, string> Makes)? Tables(string zipName)
    {
        if (_tables.TryGetValue(zipName, out var known)) return known;
        (Dictionary<ushort, string>, Dictionary<ushort, string>)? tables = null;
        if (ActiveFile(_mount, Path.Combine("stripped", "stringtables", zipName)) is { } path)
        {
            try
            {
                using var zip = ZipFile.OpenRead(path);
                tables = (ReadLsb2(Entry(zip, CarTable + ".str")), ReadLsb2(Entry(zip, MakeTable + ".str")));
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidDataException) { }
        }
        _tables[zipName] = tables;
        return tables;
    }

    // ------------------------------------------------------------------ mods

    /// <summary>The enabled mods in ExtraOptions' order with their parsed files; a mod whose files do not parse is left out, as ExtraOptions rejects it.</summary>
    private List<Mod> Mods()
    {
        if (_mods is not null) return _mods;
        var mods = new List<Mod>();
        IReadOnlyList<ExtraOptionsMod> scanned;
        try { scanned = ExtraOptionsMods.Scan(_azimuthRoot); }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidDataException or DecoderFallbackException)
        { scanned = []; } // ExtraOptions disables its loader when it cannot read loadorder.txt
        foreach (var mod in scanned.Where(m => m.Enabled && m.ManifestError is null && (m.HasDatabase || m.HasStrings)))
        {
            var directory = Path.Combine(ExtraOptionsMods.ModsDirectory(_azimuthRoot), mod.Folder);
            JsonDocument? database = null, strings = null;
            try
            {
                var (databaseFile, stringsFile) = ModFiles(directory);
                if (mod.HasDatabase && databaseFile is not null) database = ReadJson(directory, databaseFile);
                if (mod.HasStrings && stringsFile is not null) strings = ReadJson(directory, stringsFile);
                if ((database is not null && !DatabaseValid(database.RootElement)) || (strings is not null && !StringsValid(strings.RootElement)))
                    throw new InvalidDataException("rejected mod file");
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidDataException or JsonException)
            {
                database?.Dispose(); strings?.Dispose();
                continue;
            }
            mods.Add(new(mod.Folder, database, strings, database is not null && Nearest(database.RootElement)));
        }
        return _mods = mods;
    }

    /// <summary>The mod's <c>database.json</c> / <c>strings.json</c> names: its manifest's <c>database</c> / <c>strings</c> (null turns one off), else the defaults.</summary>
    private static (string? Database, string? Strings) ModFiles(string directory)
    {
        string? database = "database.json", strings = "strings.json";
        var manifest = Path.Combine(directory, "mod.json");
        if (!File.Exists(manifest)) manifest = Path.Combine(directory, "addon.json");
        if (!File.Exists(manifest)) return (database, strings);
        using var document = ReadJson(directory, Path.GetFileName(manifest));
        var root = document.RootElement;
        static string? Field(JsonElement root, string name, string? fallback) =>
            !root.TryGetProperty(name, out var value) ? fallback : value.ValueKind == JsonValueKind.String ? value.GetString() : null;
        return (Field(root, "database", database), Field(root, "strings", strings));
    }

    private static JsonDocument ReadJson(string directory, string relative)
    {
        var root = Path.GetFullPath(directory) + Path.DirectorySeparatorChar;
        var path = Path.GetFullPath(Path.Combine(directory, relative));
        if (!path.StartsWith(root, StringComparison.OrdinalIgnoreCase)) throw new InvalidDataException("mod file outside its folder");
        var info = new FileInfo(path);
        if (!info.Exists || info.Length > MaxModJsonBytes) throw new InvalidDataException("mod file missing or too large");
        return JsonDocument.Parse(File.ReadAllBytes(path), new JsonDocumentOptions { MaxDepth = 64 });
    }

    /// <summary>ExtraOptions' first checks of a <c>database.json</c>: format 1 or 2, target the stripped database, <c>tables</c> an object.</summary>
    private static bool DatabaseValid(JsonElement root) =>
        root.ValueKind == JsonValueKind.Object
        && root.TryGetProperty("format_version", out var format) && format.ValueKind == JsonValueKind.Number && format.TryGetInt32(out var version) && version is 1 or 2
        && root.TryGetProperty("target", out var target) && target.ValueKind == JsonValueKind.Object
        && target.TryGetProperty("database", out var path) && path.ValueKind == JsonValueKind.String && path.GetString() == "media/stripped/gamedbRC.slt"
        && root.TryGetProperty("tables", out var tables) && tables.ValueKind == JsonValueKind.Object;

    /// <summary>ExtraOptions' first checks of a <c>strings.json</c>: format 1 and a <c>languages</c> object.</summary>
    private static bool StringsValid(JsonElement root) =>
        root.ValueKind == JsonValueKind.Object
        && root.TryGetProperty("format_version", out var format) && format.ValueKind == JsonValueKind.Number && format.TryGetInt32(out var version) && version == 1
        && root.TryGetProperty("languages", out var languages) && languages.ValueKind == JsonValueKind.Object;

    /// <summary><c>policy.id_conflicts</c> (or the legacy top-level <c>id_conflicts</c>) is <c>"nearest"</c>.</summary>
    private static bool Nearest(JsonElement root)
    {
        static bool Is(JsonElement? value) => value is { ValueKind: JsonValueKind.String } v && v.GetString() == "nearest";
        return Is(Member(Member(root, "policy"), "id_conflicts")) || Is(Member(root, "id_conflicts"));
    }

    // ------------------------------------------------------------- LSB2 files

    private static byte[] Entry(ZipArchive zip, string name)
    {
        var entry = zip.Entries.FirstOrDefault(e => string.Equals(e.Name, name, StringComparison.OrdinalIgnoreCase))
            ?? throw new InvalidDataException("string table missing");
        if (entry.Length > MaxStringTableBytes) throw new InvalidDataException("string table too large");
        using var stream = entry.Open();
        var bytes = new byte[entry.Length];
        stream.ReadExactly(bytes);
        return bytes;
    }

    /// <summary>The key of a <c>_&amp;&lt;u32&gt;</c> string reference (its low 16 bits), or null for anything else.</summary>
    internal static ushort? StringKey(string? reference) =>
        reference is { Length: > 2 } && reference.StartsWith("_&", StringComparison.Ordinal)
        && uint.TryParse(reference.AsSpan(2), NumberStyles.None, CultureInfo.InvariantCulture, out var value)
            ? (ushort)(value & 0xffff) : null;

    /// <summary>
    /// An LSB2 <c>.str</c> table by key (tools/gamedata/extract_fh2.py <c>lsb2</c>): <c>"LSB2"</c>, the u32 LE entry count at
    /// 0x28, <c>count</c> entries of {u16 LE key, u32 LE offset} from 0x2C, then the texts at <c>0x2C + 6·count + 6</c>; each
    /// entry's text starts <c>offset</c> UTF-16 units in, UTF-16LE up to a NUL (or the end). Empty when it is not LSB2.
    /// </summary>
    internal static Dictionary<ushort, string> ReadLsb2(ReadOnlySpan<byte> table)
    {
        var strings = new Dictionary<ushort, string>();
        if (table.Length < 0x2C || !table[..4].SequenceEqual("LSB2"u8)) return strings;
        var count = BinaryPrimitives.ReadUInt32LittleEndian(table[0x28..]);
        var texts = 0x2CL + 6L * count + 6;
        for (long i = 0; i < count; i++)
        {
            var at = 0x2CL + 6 * i;
            if (at + 6 > table.Length) break;
            var key = BinaryPrimitives.ReadUInt16LittleEndian(table[(int)at..]);
            var start = texts + 2L * BinaryPrimitives.ReadUInt32LittleEndian(table[(int)(at + 2)..]);
            if (start > table.Length) continue;
            var text = table[(int)start..];
            var end = 0;
            while (end + 2 <= text.Length && (text[end] | text[end + 1]) != 0) end += 2;
            strings[key] = Encoding.Unicode.GetString(text[..end]);
        }
        return strings;
    }
}
