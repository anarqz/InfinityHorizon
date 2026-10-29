// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
using System.Buffers.Binary;
using System.Globalization;
using System.IO.Compression;
using System.Text;
using Microsoft.Data.Sqlite;

namespace Infinity.Core.Process;

/// <summary>
/// CR-L41.3 / T10 (docs/launcher/DISCORD_PRESENCE.md §2): the hook's car key (current+0x40, <c>Data_Car.Id × 1000</c>) as
/// <c>"&lt;Year&gt; &lt;Make&gt; &lt;Model&gt;"</c>, read from the player's own install only: <c>Data_Car.Year</c>,
/// <c>List_CarMake.DisplayName</c> and <c>Data_Car.DisplayName</c> of the database the game reads (read-only SQLite), the two
/// names being <c>_&amp;&lt;u32&gt;</c> references whose low 16 bits key the LSB2 tables <c>Data_Car.str</c> /
/// <c>List_CarMake.str</c> of <c>stripped/stringtables/&lt;LANG&gt;.zip</c>.
/// <para>
/// The game reads each file from the first of <c>mediaoverride</c>, <c>mediapc</c> and <c>media</c> that has it, so a mod's
/// session database (<c>mediaoverride/stripped/gamedbRC.slt</c>, which the mod runtime builds from a car pack's
/// <c>databaseBase</c> or seeds from the stock file) and a car pack's string tables win over the stock ones
/// (<see cref="ActiveFile"/>).
/// </para>
/// <para>
/// Names live only in this object's memory: never stored, logged or shipped. Any failure (no install, an unknown key, a
/// missing string, an unreadable file) is null. <see cref="Name"/> reads files on its first calls: call it off the UI thread.
/// </para>
/// </summary>
public sealed class CarNames
{
    /// <summary>A string table larger than this is not read (the game's are far smaller).</summary>
    public const long MaxStringTableBytes = 16 * 1024 * 1024;

    /// <summary>The game's media roots, highest rank first (ExtraOptions' loose files: mediaoverride, then mediapc, then media).</summary>
    public static readonly IReadOnlyList<string> MediaRoots = ["mediaoverride", "mediapc", "media"];

    private readonly string _mount;
    private readonly string? _database;
    private readonly string[] _languages;
    private readonly Lock _gate = new();
    private readonly Dictionary<int, string?> _names = [];
    private readonly Dictionary<string, (Dictionary<ushort, string> Models, Dictionary<ushort, string> Makes)?> _tables =
        new(StringComparer.OrdinalIgnoreCase);

    /// <param name="mount">The game's Mount folder (the one holding <c>media</c>).</param>
    /// <param name="language">The launcher's language: <c>pt-BR</c> reads <c>br.zip</c> first and <c>EN.zip</c> for what it lacks; anything else <c>EN.zip</c>.</param>
    /// <param name="database">The database the game reads, when the caller knows it; else <see cref="ActiveFile"/> picks it at each lookup.</param>
    public CarNames(string mount, string? language, string? database = null)
    {
        ArgumentException.ThrowIfNullOrEmpty(mount);
        _mount = mount;
        _database = database;
        _languages = language is not null && language.StartsWith("pt", StringComparison.OrdinalIgnoreCase) ? ["br.zip", "EN.zip"] : ["EN.zip"];
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

    /// <summary>Forgets every name and table (a new game session, or the install changed).</summary>
    public void Clear()
    {
        lock (_gate)
        {
            _names.Clear();
            _tables.Clear();
        }
    }

    private string? Resolve(int id)
    {
        try
        {
            if (Row(id) is not { } row) return null;
            var (year, makeReference, modelReference) = row;
            if (StringKey(makeReference) is not { } makeKey || StringKey(modelReference) is not { } modelKey) return null;
            string? make = null, model = null;
            foreach (var language in _languages)
            {
                if (Tables(language) is not { } tables) continue;
                if (make is null && tables.Makes.TryGetValue(makeKey, out var foundMake)) make = foundMake.Trim();
                if (model is null && tables.Models.TryGetValue(modelKey, out var foundModel)) model = foundModel.Trim();
            }
            return string.IsNullOrEmpty(make) || string.IsNullOrEmpty(model) ? null : $"{year.ToString(CultureInfo.InvariantCulture)} {make} {model}";
        }
        catch (Exception error) when (error is SqliteException or IOException or UnauthorizedAccessException or InvalidDataException
            or InvalidCastException or FormatException or OverflowException)
        {
            return null;
        }
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
                tables = (ReadLsb2(Entry(zip, "Data_Car.str")), ReadLsb2(Entry(zip, "List_CarMake.str")));
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException or InvalidDataException) { }
        }
        _tables[zipName] = tables;
        return tables;
    }

    private (long Year, string Make, string Model)? Row(int id)
    {
        if ((_database ?? ActiveFile(_mount, Path.Combine("stripped", "gamedbRC.slt"))) is not { } database || !File.Exists(database)) return null;
        using var connection = new SqliteConnection(new SqliteConnectionStringBuilder
        {
            DataSource = database, Mode = SqliteOpenMode.ReadOnly, Pooling = false,
        }.ToString());
        connection.Open();
        using (var pragma = connection.CreateCommand())
        {
            pragma.CommandText = "PRAGMA query_only=ON";
            pragma.ExecuteNonQuery();
        }
        using var command = connection.CreateCommand();
        command.CommandText = "SELECT Data_Car.Year, List_CarMake.DisplayName, Data_Car.DisplayName FROM Data_Car "
            + "INNER JOIN List_CarMake ON Data_Car.MakeID = List_CarMake.ID WHERE Data_Car.Id = $id LIMIT 1";
        command.Parameters.AddWithValue("$id", id);
        using var reader = command.ExecuteReader();
        if (!reader.Read() || reader.IsDBNull(0) || reader.IsDBNull(1) || reader.IsDBNull(2)) return null;
        var year = Convert.ToInt64(reader.GetValue(0), CultureInfo.InvariantCulture);
        return year is < 1 or > 9999 ? null : (year, reader.GetString(1), reader.GetString(2));
    }

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
