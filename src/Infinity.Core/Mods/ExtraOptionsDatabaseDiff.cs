// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
// The algorithm and the database.json it writes are ExtraOptions' own: a port of export_database_diff.py
// (native/extraoptions/tools/mod_diff.py, DATABASE_SOURCE; ExtraOptions is GPL-3.0, see its LICENSE).
using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using System.Text.RegularExpressions;
using Microsoft.Data.Sqlite;

namespace Infinity.Core.Mods;

/// <summary><c>--id-conflicts</c>: refuse an insert whose requested id is taken, or let ExtraOptions move it to the nearest free one.</summary>
public enum DatabaseDiffIdConflicts { Error, Nearest }

/// <summary><c>--updates</c>: changed fields of an existing row are refused, exported as guarded overrides, or left out.</summary>
public enum DatabaseDiffUpdates { Error, Override, Ignore }

/// <summary><c>--deletes</c>: removed rows are refused, exported as guarded deletions, or left out.</summary>
public enum DatabaseDiffDeletes { Error, Include, Ignore }

/// <summary><c>--conflicts</c> (<c>policy.update_conflicts</c>): a changed <c>expect</c> value rejects the override, or the later addon wins.</summary>
public enum DatabaseDiffUpdateConflicts { Error, LastWins }

/// <summary><c>--foreign-keys</c>: new dangling declared references are refused, or preserved as they are.</summary>
public enum DatabaseDiffForeignKeys { Error, Allow }

/// <summary>The exporter's command-line choices; the defaults are the Python defaults.</summary>
public sealed record ExtraOptionsDatabaseDiffOptions
{

    public DatabaseDiffIdConflicts IdConflicts { get; init; }
    public DatabaseDiffUpdates Updates { get; init; }
    public DatabaseDiffDeletes Deletes { get; init; }
    public DatabaseDiffUpdateConflicts UpdateConflicts { get; init; }
    public DatabaseDiffForeignKeys ForeignKeys { get; init; }
    /// <summary>
    /// Game-relative target database (<c>--target</c>), given by the caller (the Python default is the stripped game database,
    /// the only target the loader accepts); no game file name is built into production code.
    /// </summary>
    public required string Target { get; init; }
    /// <summary>Explicit unique keys per table (<c>--key TABLE=COL1,COL2</c>); column names are case-sensitive.</summary>
    public IReadOnlyDictionary<string, IReadOnlyList<string>>? Keys { get; init; }
    /// <summary>Export only these tables (<c>--table</c>); null or empty exports every table.</summary>
    public IReadOnlyCollection<string>? Tables { get; init; }
    /// <summary>Optional legacy <c>addon_id</c> metadata (<c>--addon-id</c>).</summary>
    public string? AddonId { get; init; }
    /// <summary><c>target.base_sha256</c>, the vanilla file's digest; the Python CLI always writes it (the loader only warns on a mismatch).</summary>
    public bool IncludeBaseSha256 { get; init; } = true;

    /// <summary>A car pack's diff as the launcher builds it: overrides, deletions, kept dangling references, nearest free ids.</summary>
    public static ExtraOptionsDatabaseDiffOptions CarPack(string target) => new()
    {
        Target = target,
        Updates = DatabaseDiffUpdates.Override, Deletes = DatabaseDiffDeletes.Include,
        ForeignKeys = DatabaseDiffForeignKeys.Allow, IdConflicts = DatabaseDiffIdConflicts.Nearest,
    };
}

/// <summary>The database.json bytes and the counts the Python prints.</summary>
public sealed record ExtraOptionsDatabaseDiffResult(
    ReadOnlyMemory<byte> Json, string VanillaSha256, int TablesScanned, int TablesChanged,
    int Inserts, int Updates, int Deletes, int ForeignKeyViolationsKept);

/// <summary>
/// Builds an ExtraOptions addon <c>database.json</c> (format 2) from a vanilla and an edited SQLite database, both opened read-only,
/// byte for byte as <c>python export_database_diff.py vanilla custom -o database.json</c> writes it, including the scratch-merge
/// validation the Python always runs (replay on an in-memory copy of vanilla, integrity check, new declared foreign-key violations,
/// every table equal to the edited one). Refusals are <see cref="ModException"/>: <see cref="ModErrors.Unsupported"/> for changes
/// the format cannot carry, <see cref="ModErrors.Conflict"/> for a merge that does not replay, <see cref="ModErrors.ExternalFiles"/>
/// for unreadable or changing inputs.
/// </summary>
public static partial class ExtraOptionsDatabaseDiff
{
    private static readonly CultureInfo Inv = CultureInfo.InvariantCulture;
    private static readonly UTF8Encoding StrictUtf8 = new(false, throwOnInvalidBytes: true);

    public static ExtraOptionsDatabaseDiffResult Export(string vanillaPath, string editedPath, ExtraOptionsDatabaseDiffOptions options, CancellationToken cancellation = default)
    {
        ArgumentException.ThrowIfNullOrEmpty(vanillaPath);
        ArgumentException.ThrowIfNullOrEmpty(editedPath);
        ArgumentNullException.ThrowIfNull(options);
        ArgumentException.ThrowIfNullOrEmpty(options.Target);
        var addonId = string.IsNullOrEmpty(options.AddonId) ? null : options.AddonId;
        if (addonId is not null && !AddonIdPattern().IsMatch(addonId)) throw Refused("Addon ID must use lowercase letters, digits and single hyphens.");
        var target = options.Target.Replace('\\', '/');
        if (target.StartsWith('/') || target.Contains(':', StringComparison.Ordinal) || target.Split('/').Any(part => part is "" or "." or ".."))
            throw Refused("--target must be a relative game content path without traversal.");
        if (options.Keys is { } requested && requested.Any(pair => pair.Key.Length == 0 || pair.Value is null))
            throw Refused("Invalid --key.");
        string[] inputs = [Path.GetFullPath(vanillaPath), Path.GetFullPath(editedPath)];
        try
        {
            using var vanilla = OpenReadOnly(inputs[0]);
            using var custom = OpenReadOnly(inputs[1]);
            string[] hashes = [Sha256(inputs[0]), Sha256(inputs[1])];
            var plan = Compare(vanilla, custom, options, cancellation);
            var kept = Validate(vanilla, custom, plan, options, cancellation);
            if (Sha256(inputs[0]) != hashes[0] || Sha256(inputs[1]) != hashes[1])
                throw new ModException("An input changed during comparison. Close its editor/game and retry.", ModErrors.ExternalFiles);
            var json = new StringBuilder();
            Write(json, Document(plan, options, target, addonId, hashes[0]), 0, JsonMode.Indented);
            json.Append('\n');
            return new ExtraOptionsDatabaseDiffResult(StrictUtf8.GetBytes(json.ToString()), hashes[0], plan.Scanned, plan.Tables.Count,
                plan.Tables.Sum(t => t.Inserts.Count), plan.Tables.Sum(t => t.Updates.Count), plan.Tables.Sum(t => t.Deletes.Count), kept);
        }
        catch (SqliteException e) { throw new ModException("database diff: " + e.Message, ModErrors.ExternalFiles); }
        catch (IOException e) { throw new ModException("database diff: " + e.Message, ModErrors.ExternalFiles); }
        catch (UnauthorizedAccessException e) { throw new ModException("database diff: " + e.Message, ModErrors.ExternalFiles); }
        catch (DecoderFallbackException) { throw new ModException("database diff: text that is not UTF-8", ModErrors.Unsupported); }
    }

    // ---- plan ----

    private sealed record UpdateOp(Fields Where, Fields Set, Fields Expect);
    private sealed record DeleteOp(Fields Where, Fields Expect);

    private sealed class TablePlan(string name, string[] columns, string[] key)
    {
        public string Name { get; } = name;
        public string[] Columns { get; } = columns;
        public string[] Key { get; } = key;
        public List<Fields> Inserts { get; } = [];
        public List<UpdateOp> Updates { get; } = [];
        public List<DeleteOp> Deletes { get; } = [];
        /// <summary>Rows whose foreign keys need their parent records (Python <c>written</c>): inserts in read order, then overrides.</summary>
        public List<object?[]> Written { get; } = [];
    }

    private sealed class Plan
    {
        public List<TablePlan> Tables { get; } = [];
        public List<(string Table, List<Fields> Rows)> Records { get; set; } = [];
        public HashSet<string> Ignored { get; } = new(StringComparer.Ordinal);
        public HashSet<string>? Selected { get; set; }
        public int Scanned { get; set; }
    }

    /// <summary>An ordered JSON object (Python dict); values are null, long, double, string, byte[] (a <c>blob_hex</c> object), or nested.</summary>
    private sealed class Fields : List<KeyValuePair<string, object?>>
    {
        public Fields() { }
        public Fields(string[] columns, object?[] row, IEnumerable<int> indices) { foreach (var i in indices) Add(columns[i], row[i]); }
        public void Add(string name, object? value) => Add(new KeyValuePair<string, object?>(name, value));
        public object? this[string name] => this.First(pair => pair.Key == name).Value;
    }

    private static Plan Compare(SqliteConnection vanilla, SqliteConnection custom, ExtraOptionsDatabaseDiffOptions options, CancellationToken cancellation)
    {
        var oldSchema = Schema(vanilla);
        var newSchema = DataViewSchema(vanilla, custom, oldSchema, Schema(custom));
        var oldMap = oldSchema.ToDictionary(e => e.Key, e => e.Value);
        if (oldMap.Count != newSchema.Count || oldMap.Any(e => !newSchema.TryGetValue(e.Key, out var v) || v != e.Value))
        {
            var different = oldMap.Keys.Union(newSchema.Keys)
                .Where(k => !oldMap.TryGetValue(k, out var a) || !newSchema.TryGetValue(k, out var b) || a != b)
                .Select(k => k.Name).Distinct(StringComparer.Ordinal).Order(CodePoints.Instance);
            throw Refused("Schema changes are not data overrides: " + string.Join(", ", different));
        }
        var allTables = oldSchema.Where(e => e.Key.Kind == "table").Select(e => e.Key.Name).Order(CodePoints.Instance).ToList();
        var keys = options.Keys ?? new Dictionary<string, IReadOnlyList<string>>();
        var plan = new Plan { Selected = options.Tables is { Count: > 0 } t ? t.ToHashSet(StringComparer.Ordinal) : null };
        var selected = plan.Selected ?? allTables.ToHashSet(StringComparer.Ordinal);
        var unknown = selected.Union(keys.Keys).Except(allTables, StringComparer.Ordinal).Order(CodePoints.Instance).ToList();
        if (unknown.Count > 0) throw Refused("Unknown table(s): " + string.Join(", ", unknown));
        plan.Scanned = selected.Count;
        foreach (var table in allTables)
        {
            if (!selected.Contains(table)) continue;
            cancellation.ThrowIfCancellationRequested();
            var columns = ColumnsFor(vanilla, table);
            var before = ReadRows(vanilla, table, columns);
            var after = ReadRows(custom, table, columns);
            var key = ChooseKey(vanilla, table, columns, keys);
            var keyAt = key.Select(name => Array.IndexOf(columns, name)).ToArray();
            List<object?[]> added, removed;
            var changed = new List<(object?[] Previous, object?[] Current, int[] Fields)>();
            if (key.Length > 0)
            {
                var old = Indexed(before, keyAt, table, key);
                var current = Indexed(after, keyAt, table, key);
                added = current.Order.Where(e => !old.Map.ContainsKey(e.Id)).Select(e => e.Row).ToList();
                removed = old.Order.Where(e => !current.Map.ContainsKey(e.Id)).Select(e => e.Row).ToList();
                foreach (var (id, previous) in old.Order)
                {
                    if (!current.Map.TryGetValue(id, out var now)) continue;
                    var fields = Enumerable.Range(0, columns.Length).Where(i => !Same(previous[i], now[i])).ToArray();
                    if (fields.Length > 0) changed.Add((previous, now, fields));
                }
            }
            else
            {
                (added, removed) = UnkeyedDifference(before, after, columns);
                if (removed.Count > 0 && options.Deletes != DatabaseDiffDeletes.Ignore)
                    throw Refused($"{table}: {removed.Count} removed/replaced row(s), but no stable key. Use --key TABLE=Column1,Column2. Row order/rowid is never used as identity.");
            }
            if (changed.Count > 0 && options.Updates == DatabaseDiffUpdates.Error)
                throw Refused($"{table}: {changed.Count} existing row(s) changed. Use --updates override to export field overrides, or --updates ignore.");
            if (removed.Count > 0 && options.Deletes == DatabaseDiffDeletes.Error)
                throw Refused($"{table}: {removed.Count} row(s) removed. Use --deletes include to export explicit deletions, or --deletes ignore.");
            var tablePlan = new TablePlan(table, columns, key);
            var all = Enumerable.Range(0, columns.Length).ToArray();
            if (added.Count > 0) tablePlan.Inserts.AddRange(Ordered(added, columns).Select(row => new Fields(columns, row, all)));
            var overrides = changed.Count > 0 && options.Updates == DatabaseDiffUpdates.Override;
            if (overrides)
            {
                changed = changed.OrderBy(item => Text(new Fields(columns, item.Previous, keyAt), JsonMode.Default), StringComparer.Ordinal).ToList();
                tablePlan.Updates.AddRange(changed.Select(item => new UpdateOp(new Fields(columns, item.Previous, keyAt),
                    new Fields(columns, item.Current, item.Fields), new Fields(columns, item.Previous, item.Fields))));
            }
            if (removed.Count > 0 && options.Deletes == DatabaseDiffDeletes.Include)
                tablePlan.Deletes.AddRange(Ordered(removed, columns).Select(row => new DeleteOp(new Fields(columns, row, keyAt), new Fields(columns, row, all))));
            if (changed.Count > 0 && options.Updates == DatabaseDiffUpdates.Ignore || removed.Count > 0 && options.Deletes == DatabaseDiffDeletes.Ignore)
                plan.Ignored.Add(table);
            if (tablePlan.Inserts.Count + tablePlan.Updates.Count + tablePlan.Deletes.Count > 0)
            {
                tablePlan.Written.AddRange(added);
                if (overrides) tablePlan.Written.AddRange(changed.Select(item => item.Current));
                plan.Tables.Add(tablePlan);
            }
        }
        plan.Records = RequiredRecords(vanilla, plan.Tables);
        return plan;
    }

    private static Fields Document(Plan plan, ExtraOptionsDatabaseDiffOptions options, string target, string? addonId, string baseHash)
    {
        var targetObject = new Fields { { "database", target } };
        if (options.IncludeBaseSha256) targetObject.Add("base_sha256", baseHash);
        var records = new Fields();
        foreach (var (table, rows) in plan.Records) records.Add(table, rows.Cast<object?>().ToList());
        var tables = new Fields();
        foreach (var table in plan.Tables)
        {
            var operations = new Fields { { "key", table.Key.Cast<object?>().ToList() } };
            if (table.Inserts.Count > 0) operations.Add("insert", table.Inserts.Cast<object?>().ToList());
            if (table.Updates.Count > 0)
                operations.Add("update", table.Updates.Select(u => (object?)new Fields { { "where", u.Where }, { "set", u.Set }, { "expect", u.Expect } }).ToList());
            if (table.Deletes.Count > 0)
                operations.Add("delete", table.Deletes.Select(d => (object?)new Fields { { "where", d.Where }, { "expect", d.Expect } }).ToList());
            tables.Add(table.Name, operations);
        }
        var document = new Fields
        {
            { "format_version", 2L },
            { "target", targetObject },
            { "policy", new Fields {
                { "id_conflicts", options.IdConflicts == DatabaseDiffIdConflicts.Nearest ? "nearest" : "error" },
                { "update_conflicts", options.UpdateConflicts == DatabaseDiffUpdateConflicts.LastWins ? "last_wins" : "error" },
                { "foreign_keys", options.ForeignKeys == DatabaseDiffForeignKeys.Allow ? "allow" : "error" } } },
            { "requires", new Fields { { "records", records } } },
            { "tables", tables },
        };
        if (addonId is not null) document.Add("addon_id", addonId);
        return document;
    }

    /// <summary>Parent rows the addon relies on: every update/delete selector, and each existing parent a written row references.</summary>
    private static List<(string, List<Fields>)> RequiredRecords(SqliteConnection vanilla, List<TablePlan> tables)
    {
        var records = new Dictionary<string, (List<Fields> Rows, HashSet<string> Seen)>(StringComparer.Ordinal);
        void Require(string table, Fields row)
        {
            if (!records.TryGetValue(table, out var entry)) records[table] = entry = ([], new HashSet<string>(StringComparer.Ordinal));
            var sorted = new Fields();
            sorted.AddRange(row.OrderBy(pair => pair.Key, StringComparer.Ordinal));
            if (entry.Seen.Add(Text(sorted, JsonMode.Default))) entry.Rows.Add(row);
        }
        foreach (var table in tables)
        {
            foreach (var update in table.Updates) Require(table.Name, update.Where);
            foreach (var delete in table.Deletes) Require(table.Name, delete.Where);
            var groups = new List<List<object?[]>>();
            foreach (var row in Query(vanilla, $"PRAGMA foreign_key_list({Identifier(table.Name)})"))
            {
                var group = groups.Find(g => Same(g[0][0], row[0]));
                if (group is null) groups.Add([row]); else group.Add(row);
            }
            foreach (var unsorted in groups)
            {
                var group = unsorted.OrderBy(row => (long)row[1]!).ToList();
                var parent = (string)group[0][2]!;
                var parentKey = PrimaryKey(vanilla, parent);
                var destinations = group.Select(row => row[4] as string ?? ((long)row[1]! < parentKey.Count ? parentKey[(int)(long)row[1]!] : null)).ToList();
                if (destinations.Any(name => name is null)) throw Refused($"{table.Name}: cannot resolve foreign key to {parent}.");
                var from = group.Select(row => Array.IndexOf(table.Columns, (string)row[3]!)).ToArray();
                if (from.Any(i => i < 0)) throw Refused($"{table.Name}: foreign key column not found.");
                var where = string.Join(" AND ", destinations.Select((name, i) => $"{Identifier(name!)} IS @p{i}"));
                var sql = $"SELECT 1 FROM {Identifier(parent)} WHERE {where} LIMIT 1";
                foreach (var row in table.Written)
                {
                    var values = from.Select(i => row[i]).ToArray();
                    if (values.Any(value => value is null) || Query(vanilla, sql, values).Count == 0) continue;
                    var reference = new Fields();
                    for (var i = 0; i < values.Length; i++)
                    {
                        var at = reference.FindIndex(pair => pair.Key == destinations[i]);
                        if (at >= 0) reference[at] = new(destinations[i]!, values[i]); else reference.Add(destinations[i]!, values[i]);
                    }
                    Require(parent, reference);
                }
            }
        }
        return records.Keys.Order(CodePoints.Instance).Select(table => (table, records[table].Rows)).ToList();
    }

    // ---- scratch merge (Python validate/replay) ----

    private static int Validate(SqliteConnection vanilla, SqliteConnection custom, Plan plan, ExtraOptionsDatabaseDiffOptions options, CancellationToken cancellation)
    {
        using var merged = new SqliteConnection(new SqliteConnectionStringBuilder { DataSource = ":memory:", Pooling = false, ForeignKeys = false }.ToString());
        merged.Open();
        vanilla.BackupDatabase(merged);
        var baseline = Multiset(Query(merged, "PRAGMA foreign_key_check"));
        Execute(merged, "BEGIN");
        Replay(merged, plan, options);
        if (!IsOk(merged)) throw new ModException("Scratch merge failed SQLite integrity checks.", ModErrors.Conflict);
        var violations = Multiset(Query(merged, "PRAGMA foreign_key_check"));
        var added = violations.Sum(pair => Math.Max(0, pair.Value - baseline.GetValueOrDefault(pair.Key)));
        if (added > 0 && options.ForeignKeys != DatabaseDiffForeignKeys.Allow)
            throw new ModException($"Scratch merge introduces {added} declared foreign-key violations. Use --foreign-keys allow to preserve these references as-is.", ModErrors.Conflict);
        foreach (var entry in Schema(vanilla))
        {
            if (entry.Key.Kind != "table" || plan.Ignored.Contains(entry.Key.Name)) continue;
            cancellation.ThrowIfCancellationRequested();
            var table = entry.Key.Name;
            var expected = plan.Selected is null || plan.Selected.Contains(table) ? custom : vanilla;
            var columns = ColumnsFor(vanilla, table);
            var mine = Multiset(ReadRows(merged, table, columns));
            var theirs = Multiset(ReadRows(expected, table, columns));
            if (mine.Count != theirs.Count || mine.Any(pair => theirs.GetValueOrDefault(pair.Key) != pair.Value))
                throw new ModException($"{table}: scratch merge differs from the expected database.", ModErrors.Conflict);
        }
        Execute(merged, "COMMIT");
        return added;
    }

    private static void Replay(SqliteConnection connection, Plan plan, ExtraOptionsDatabaseDiffOptions options)
    {
        foreach (var table in plan.Tables)
        {
            var name = Identifier(table.Name);
            foreach (var (selector, expect, set) in table.Deletes.Select(d => (d.Where, d.Expect, (Fields?)null))
                         .Concat(table.Updates.Select(u => (u.Where, u.Expect, (Fields?)u.Set))))
            {
                if (selector.Count == 0 || !selector.Select(pair => pair.Key).SequenceEqual(table.Key, StringComparer.Ordinal))
                    throw Refused($"{table.Name}: operation must select its complete declared key.");
                var where = string.Join(" AND ", selector.Select((pair, i) => $"{Identifier(pair.Key)} IS @p{i}"));
                var values = selector.Select(pair => pair.Value).ToArray();
                var (names, matches) = QueryNamed(connection, $"SELECT * FROM {name} WHERE {where}", values);
                if (matches.Count != 1)
                    throw new ModException($"{table.Name}: override/delete selector must match exactly one row.", ModErrors.Conflict);
                if (options.UpdateConflicts == DatabaseDiffUpdateConflicts.Error)
                    foreach (var (column, expected) in expect)
                    {
                        var at = Array.IndexOf(names, column);
                        if (at < 0 || !Same(matches[0][at], expected))
                            throw new ModException($"{table.Name}: conflicting change to {column}.", ModErrors.Conflict);
                    }
                if (set is null) Execute(connection, $"DELETE FROM {name} WHERE {where}", values);
                else
                {
                    var assignments = string.Join(",", set.Select((pair, i) => $"{Identifier(pair.Key)}=@p{i}"));
                    var shifted = string.Join(" AND ", selector.Select((pair, i) => $"{Identifier(pair.Key)} IS @p{set.Count + i}"));
                    Execute(connection, $"UPDATE {name} SET {assignments} WHERE {shifted}", [.. set.Select(pair => pair.Value), .. values]);
                }
            }
            foreach (var row in table.Inserts)
            {
                if (table.Key.Length > 0)
                {
                    var where = string.Join(" AND ", table.Key.Select((column, i) => $"{Identifier(column)} IS @p{i}"));
                    if (Query(connection, $"SELECT 1 FROM {name} WHERE {where}", table.Key.Select(column => row[column]).ToArray()).Count > 0)
                        throw new ModException($"{table.Name}: insert key already exists; inserts never replace rows.", ModErrors.Conflict);
                }
                var columns = string.Join(",", row.Select(pair => Identifier(pair.Key)));
                var placeholders = string.Join(",", row.Select((_, i) => "@p" + i.ToString(Inv)));
                Execute(connection, $"INSERT INTO {name} ({columns}) VALUES ({placeholders})", row.Select(pair => pair.Value).ToArray());
            }
        }
    }

    // ---- schema, keys, rows ----

    private readonly record struct SchemaKey(string Kind, string Name);
    private readonly record struct SchemaEntry(string? Table, string? Sql);

    private static List<KeyValuePair<SchemaKey, SchemaEntry>> Schema(SqliteConnection connection) =>
        Query(connection, "SELECT type,name,tbl_name,sql FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'")
            .Select(row => new KeyValuePair<SchemaKey, SchemaEntry>(new((string)row[0]!, (string)row[1]!), new(row[2] as string, row[3] as string)))
            .ToList();

    /// <summary>A table replaced by <c>CREATE VIEW t AS SELECT * FROM backing WHERE …</c> over an identical new table is a row snapshot, not a schema change.</summary>
    private static Dictionary<SchemaKey, SchemaEntry> DataViewSchema(SqliteConnection vanilla, SqliteConnection custom,
        List<KeyValuePair<SchemaKey, SchemaEntry>> oldSchema, List<KeyValuePair<SchemaKey, SchemaEntry>> newSchemaList)
    {
        var newSchema = newSchemaList.ToDictionary(e => e.Key, e => e.Value);
        var normalized = new Dictionary<SchemaKey, SchemaEntry>(newSchema);
        static string? Body(string? sql) => sql is not null && sql.Contains('(', StringComparison.Ordinal) ? sql[sql.IndexOf('(', StringComparison.Ordinal)..].Trim().TrimEnd(';').Trim() : null;
        foreach (var (key, definition) in oldSchema)
        {
            var table = key.Name;
            if (key.Kind != "table" || newSchema.ContainsKey(new("table", table)) || !newSchema.TryGetValue(new("view", table), out var view)) continue;
            var match = ViewPattern().Match(view.Sql ?? "");
            if (!match.Success) continue;
            var backing = match.Groups[1].Value;
            if (backing[0] is '"' or '[' or '`') backing = backing[1..^1].Replace("\"\"", "\"", StringComparison.Ordinal);
            if (!newSchema.TryGetValue(new("table", backing), out var backingDefinition) || oldSchema.Any(e => e.Key.Name == backing)) continue;
            var oldInfo = TableInfo(vanilla, table);
            if (Body(definition.Sql) != Body(backingDefinition.Sql) || !SameRows(oldInfo, TableInfo(custom, backing))
                || !SameRows(oldInfo.Select(r => new[] { r[1], r[2], r[6] }).ToList(), TableInfo(custom, table).Select(r => new[] { r[1], r[2], r[6] }).ToList()))
                continue;
            if (!normalized.ContainsKey(new("table", backing))) continue; // one backing table cannot satisfy two replacements
            normalized.Remove(new("view", table));
            normalized.Remove(new("table", backing));
            normalized[new("table", table)] = definition;
        }
        return normalized;
    }

    private static List<object?[]> TableInfo(SqliteConnection connection, string table) => Query(connection, $"PRAGMA table_xinfo({Identifier(table)})");

    private static List<string> PrimaryKey(SqliteConnection connection, string table) =>
        TableInfo(connection, table).OrderBy(row => (long)row[5]!).Where(row => (long)row[5]! != 0).Select(row => (string)row[1]!).ToList();

    private static string[] ColumnsFor(SqliteConnection connection, string table)
    {
        var info = TableInfo(connection, table);
        if (info.Any(row => (long)row[6]! != 0)) throw Refused($"{table}: generated/hidden columns are not supported.");
        return info.Select(row => (string)row[1]!).ToArray();
    }

    private static string[] ChooseKey(SqliteConnection connection, string table, string[] columns, IReadOnlyDictionary<string, IReadOnlyList<string>> requested)
    {
        if (requested.TryGetValue(table, out var explicitKey))
        {
            if (explicitKey.Count == 0 || explicitKey.Distinct(StringComparer.Ordinal).Count() != explicitKey.Count || explicitKey.Any(name => !columns.Contains(name, StringComparer.Ordinal)))
                throw Refused($"{table}: invalid explicit key. Column names are case-sensitive.");
            return [.. explicitKey];
        }
        var primary = PrimaryKey(connection, table);
        if (primary.Count > 0) return [.. primary];
        foreach (var index in Query(connection, $"PRAGMA index_list({Identifier(table)})"))
        {
            if ((long)index[2]! == 0 || (long)index[4]! != 0) continue; // unique, non-partial indexes only
            var key = Query(connection, $"PRAGMA index_info({Identifier((string)index[1]!)})").Select(row => row[2] as string).ToList();
            if (key.Count > 0 && key.All(name => name is not null && columns.Contains(name, StringComparer.Ordinal))) return [.. key.Select(name => name!)];
        }
        return [];
    }

    private static List<object?[]> ReadRows(SqliteConnection connection, string table, string[] columns) =>
        Query(connection, $"SELECT {string.Join(",", columns.Select(Identifier))} FROM {Identifier(table)}");

    private static (List<(string Id, object?[] Row)> Order, Dictionary<string, object?[]> Map) Indexed(List<object?[]> rows, int[] keyAt, string table, string[] key)
    {
        var order = new List<(string, object?[])>();
        var map = new Dictionary<string, object?[]>(StringComparer.Ordinal);
        foreach (var row in rows)
        {
            var id = Identity(row, keyAt);
            if (keyAt.Any(i => row[i] is null) || !map.TryAdd(id, row))
                throw Refused($"{table}: key [{string.Join(", ", key)}] contains NULL or duplicate values. Use --key TABLE=Column1,Column2 to specify a stable unique key.");
            order.Add((id, row));
        }
        return (order, map);
    }

    private static (List<object?[]> Added, List<object?[]> Removed) UnkeyedDifference(List<object?[]> before, List<object?[]> after, string[] columns)
    {
        var all = Enumerable.Range(0, columns.Length).ToArray();
        static List<(string Id, object?[] Row, int Count)> Count(List<object?[]> rows, int[] all)
        {
            var order = new List<(string Id, object?[] Row, int Count)>();
            var at = new Dictionary<string, int>(StringComparer.Ordinal);
            foreach (var row in rows)
            {
                var id = Identity(row, all);
                if (at.TryGetValue(id, out var i)) order[i] = (id, order[i].Row, order[i].Count + 1);
                else { at[id] = order.Count; order.Add((id, row, 1)); }
            }
            return order;
        }
        var old = Count(before, all);
        var current = Count(after, all);
        List<object?[]> Minus(List<(string Id, object?[] Row, int Count)> a, List<(string Id, object?[] Row, int Count)> b)
        {
            var other = b.ToDictionary(e => e.Id, e => e.Count, StringComparer.Ordinal);
            var rows = new List<object?[]>();
            foreach (var (id, row, count) in a)
                for (var n = count - other.GetValueOrDefault(id); n > 0; n--) rows.Add(row);
            return Ordered(rows, columns);
        }
        return (Minus(current, old), Minus(old, current));
    }

    /// <summary>Python <c>ordered</c>: rows sorted (stably) by their compact ASCII JSON.</summary>
    private static List<object?[]> Ordered(List<object?[]> rows, string[] columns)
    {
        var all = Enumerable.Range(0, columns.Length).ToArray();
        return rows.Select(row => (Row: row, Text: Text(new Fields(columns, row, all), JsonMode.Compact)))
            .OrderBy(e => e.Text, StringComparer.Ordinal).Select(e => e.Row).ToList();
    }

    private static Dictionary<string, int> Multiset(List<object?[]> rows)
    {
        var counts = new Dictionary<string, int>(StringComparer.Ordinal);
        foreach (var row in rows)
        {
            var id = Identity(row, Enumerable.Range(0, row.Length).ToArray());
            counts[id] = counts.GetValueOrDefault(id) + 1;
        }
        return counts;
    }

    // ---- values: Python typed() identity ----

    /// <summary>Python <c>(type(value).__name__, value)</c> equality: 1, 1.0 and '1' differ; 0.0 equals -0.0.</summary>
    private static bool Same(object? a, object? b) => (a, b) switch
    {
        (null, null) => true,
        (long x, long y) => x == y,
        (double x, double y) => x.Equals(y) || x == y,
        (string x, string y) => string.Equals(x, y, StringComparison.Ordinal),
        (byte[] x, byte[] y) => x.AsSpan().SequenceEqual(y),
        _ => false,
    };

    private static bool SameRows(List<object?[]> a, List<object?[]> b) => a.Count == b.Count && a.Zip(b).All(p => p.First.Length == p.Second.Length && p.First.Zip(p.Second).All(v => Same(v.First, v.Second)));

    private static string Identity(object?[] row, int[] indices)
    {
        var s = new StringBuilder();
        foreach (var i in indices)
        {
            switch (row[i])
            {
                case null: s.Append('n'); break;
                case long l: s.Append('i').Append(l.ToString(Inv)); break;
                case double d: s.Append('f').Append(BitConverter.DoubleToInt64Bits(d == 0 ? 0d : d).ToString(Inv)); break;
                case string t: s.Append('s').Append(t.Length.ToString(Inv)).Append(':').Append(t); break;
                case byte[] b: s.Append('b').Append(Convert.ToHexString(b)); break;
                default: throw new InvalidOperationException("unexpected SQLite value");
            }
            s.Append(';');
        }
        return s.ToString();
    }

    // ---- SQLite access ----

    private static string Identifier(string name) => "\"" + name.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"";

    private static SqliteConnection OpenReadOnly(string path)
    {
        if (!File.Exists(path)) throw new ModException("Database not found: " + Path.GetFileName(path), ModErrors.ExternalFiles);
        var wal = new FileInfo(path + "-wal");
        if (wal.Exists && wal.Length > 0)
            throw new ModException(Path.GetFileName(path) + ": active WAL sidecar found. Export a checkpointed, closed database first.", ModErrors.ExternalFiles);
        var connection = new SqliteConnection(new SqliteConnectionStringBuilder { DataSource = path, Mode = SqliteOpenMode.ReadOnly, Pooling = false }.ToString());
        try
        {
            connection.Open();
            Execute(connection, "PRAGMA query_only=ON");
            Execute(connection, "PRAGMA trusted_schema=OFF");
            if (!IsOk(connection)) throw new ModException("Invalid SQLite database: " + Path.GetFileName(path), ModErrors.ExternalFiles);
            Execute(connection, "BEGIN"); // one read snapshot for the whole export
            return connection;
        }
        catch
        {
            connection.Dispose();
            throw;
        }
    }

    private static bool IsOk(SqliteConnection connection) => Query(connection, "PRAGMA quick_check") is [[string ok]] && ok == "ok";

    private static string Sha256(string path)
    {
        using var stream = new FileStream(path, FileMode.Open, FileAccess.Read, FileShare.ReadWrite | FileShare.Delete);
        return Convert.ToHexStringLower(SHA256.HashData(stream));
    }

    private static SqliteCommand Command(SqliteConnection connection, string sql, IReadOnlyList<object?>? args)
    {
        var command = connection.CreateCommand();
#pragma warning disable CA2100 // identifiers are quoted with Identifier(); every value is a bound parameter
        command.CommandText = sql;
#pragma warning restore CA2100
        for (var i = 0; args is not null && i < args.Count; i++) command.Parameters.AddWithValue("@p" + i.ToString(Inv), args[i] ?? DBNull.Value);
        return command;
    }

    private static void Execute(SqliteConnection connection, string sql, IReadOnlyList<object?>? args = null)
    {
        using var command = Command(connection, sql, args);
        command.ExecuteNonQuery();
    }

    private static List<object?[]> Query(SqliteConnection connection, string sql, IReadOnlyList<object?>? args = null) => QueryNamed(connection, sql, args).Rows;

    private static (string[] Names, List<object?[]> Rows) QueryNamed(SqliteConnection connection, string sql, IReadOnlyList<object?>? args)
    {
        using var command = Command(connection, sql, args);
        using var reader = command.ExecuteReader();
        var names = Enumerable.Range(0, reader.FieldCount).Select(reader.GetName).ToArray();
        var rows = new List<object?[]>();
        while (reader.Read())
        {
            var row = new object?[reader.FieldCount];
            for (var i = 0; i < row.Length; i++)
            {
                if (reader.IsDBNull(i)) continue;
                var value = reader.GetValue(i);
                // TEXT as its exact bytes, decoded strictly like Python's sqlite3 (embedded NULs kept, invalid UTF-8 refused).
                row[i] = value is string ? StrictUtf8.GetString(reader.GetFieldValue<byte[]>(i)) : value;
            }
            rows.Add(row);
        }
        return (names, rows);
    }

    // ---- JSON exactly as Python's json module writes it ----

    /// <summary>Indented: <c>json.dump(indent=2, ensure_ascii=False)</c>; Default: <c>json.dumps(x)</c>; Compact: <c>separators=(',', ':')</c>.</summary>
    private enum JsonMode { Indented, Default, Compact }

    private static string Text(object? value, JsonMode mode)
    {
        var s = new StringBuilder();
        Write(s, value, 0, mode);
        return s.ToString();
    }

    private static void Write(StringBuilder s, object? value, int level, JsonMode mode)
    {
        switch (value)
        {
            case null: s.Append("null"); break;
            case long l: s.Append(l.ToString(Inv)); break;
            case double d: s.Append(PythonFloat(d)); break;
            case string t: Quote(s, t, mode != JsonMode.Indented); break;
            case byte[] b: Write(s, new Fields { { "blob_hex", Convert.ToHexStringLower(b) } }, level, mode); break;
            case Fields o:
                Container(s, '{', '}', o.Count, level, mode, i =>
                {
                    Quote(s, o[i].Key, mode != JsonMode.Indented);
                    s.Append(mode == JsonMode.Compact ? ":" : ": ");
                    Write(s, o[i].Value, level + 1, mode);
                });
                break;
            case List<object?> a: Container(s, '[', ']', a.Count, level, mode, i => Write(s, a[i], level + 1, mode)); break;
            default: throw new InvalidOperationException("unexpected JSON value");
        }
    }

    private static void Container(StringBuilder s, char open, char close, int count, int level, JsonMode mode, Action<int> item)
    {
        s.Append(open);
        if (count == 0) { s.Append(close); return; }
        for (var i = 0; i < count; i++)
        {
            if (i > 0) s.Append(mode == JsonMode.Default ? ", " : ",");
            if (mode == JsonMode.Indented) s.Append('\n').Append(' ', (level + 1) * 2);
            item(i);
        }
        if (mode == JsonMode.Indented) s.Append('\n').Append(' ', level * 2);
        s.Append(close);
    }

    private static void Quote(StringBuilder s, string text, bool ascii)
    {
        s.Append('"');
        foreach (var c in text)
        {
            switch (c)
            {
                case '"': s.Append("\\\""); break;
                case '\\': s.Append("\\\\"); break;
                case '\n': s.Append("\\n"); break;
                case '\r': s.Append("\\r"); break;
                case '\t': s.Append("\\t"); break;
                case '\b': s.Append("\\b"); break;
                case '\f': s.Append("\\f"); break;
                default:
                    if (c < ' ' || ascii && c > '~') s.Append("\\u").Append(((int)c).ToString("x4", Inv));
                    else s.Append(c);
                    break;
            }
        }
        s.Append('"');
    }

    /// <summary>Python <c>repr(float)</c>: the shortest round-trip digits, positional for exponents -4..15, else <c>1e+16</c> style.</summary>
    internal static string PythonFloat(double value)
    {
        if (!double.IsFinite(value)) throw Refused("Non-finite numbers cannot be represented in addon JSON.");
        if (value == 0) return double.IsNegative(value) ? "-0.0" : "0.0";
        var r = value.ToString("R", Inv);
        var negative = r[0] == '-';
        if (negative) r = r[1..];
        var exponent = 0;
        var e = r.IndexOf('E', StringComparison.Ordinal);
        if (e >= 0) { exponent = int.Parse(r[(e + 1)..], NumberStyles.AllowLeadingSign, Inv); r = r[..e]; }
        var dot = r.IndexOf('.', StringComparison.Ordinal);
        var whole = dot < 0 ? r : r[..dot];
        var digits = whole + (dot < 0 ? "" : r[(dot + 1)..]);
        var point = whole.Length + exponent; // value = 0.digits × 10^point
        var zeros = digits.Length - digits.TrimStart('0').Length;
        digits = digits[zeros..].TrimEnd('0');
        point -= zeros;
        string text;
        if (point is > -4 and <= 16)
        {
            if (point <= 0) text = "0." + new string('0', -point) + digits;
            else if (point >= digits.Length) text = digits + new string('0', point - digits.Length) + ".0";
            else text = digits[..point] + "." + digits[point..];
        }
        else
        {
            var power = point - 1;
            text = digits[..1] + (digits.Length > 1 ? "." + digits[1..] : "") + "e" + (power < 0 ? "-" : "+") + Math.Abs(power).ToString("00", Inv);
        }
        return negative ? "-" + text : text;
    }

    private static ModException Refused(string message) => new("database diff: " + message, ModErrors.Unsupported);

    /// <summary>Python's <c>sorted()</c> over strings: Unicode code points, not UTF-16 units.</summary>
    private sealed class CodePoints : IComparer<string>
    {
        public static CodePoints Instance { get; } = new();
        public int Compare(string? x, string? y)
        {
            if (x is null || y is null) return string.CompareOrdinal(x, y);
            using var a = x.EnumerateRunes().GetEnumerator();
            using var b = y.EnumerateRunes().GetEnumerator();
            while (true)
            {
                bool hasA = a.MoveNext(), hasB = b.MoveNext();
                if (!hasA || !hasB) return hasA.CompareTo(hasB);
                var c = a.Current.Value.CompareTo(b.Current.Value);
                if (c != 0) return c;
            }
        }
    }

    [GeneratedRegex("^[a-z0-9]+(?:-[a-z0-9]+)*\\z", RegexOptions.CultureInvariant)]
    private static partial Regex AddonIdPattern();

    private const string NamePattern = "(?:\"(?:[^\"]|\"\")+\"|\\[[^\\]]+\\]|`[^`]+`|[A-Za-z_][A-Za-z_0-9]*)";

    [GeneratedRegex("\\A(?:^\\s*CREATE\\s+VIEW\\s+" + NamePattern + "\\s+AS\\s+SELECT\\s+\\*\\s+FROM\\s+(" + NamePattern + ")\\s+WHERE\\s+.+?\\s*;?\\s*$)\\z",
        RegexOptions.IgnoreCase | RegexOptions.Singleline | RegexOptions.CultureInvariant)]
    private static partial Regex ViewPattern();
}
