// Copyright (C) 2026 Alexandre Moraes and contributors. GPL-3.0-only; see LICENSE and NOTICE.
namespace Infinity.Core.Saves;

/// <summary>The sibling containers a career revision names (<see cref="SaveLayout.References"/>): what a career bundle packs beside it (CR-S105).</summary>
public static class SaveReferences
{
    /// <summary>
    /// The sibling container names <paramref name="revision"/>'s career rows point to (<see cref="SaveLayout.References"/>; empty when the
    /// game names none). The data file is read from a private temporary copy past its header, read-only; a file that is not the database
    /// the layout says throws <see cref="SaveException"/> (never guessed around). Values that are not a sibling name are not references.
    /// </summary>
    public static IReadOnlySet<string> ReferencedSiblings(LocalRevision revision, SaveLayout layout)
    {
        ArgumentNullException.ThrowIfNull(revision);
        ArgumentNullException.ThrowIfNull(layout);
        if (layout.References is not { } refs) return new HashSet<string>(StringComparer.Ordinal);
        byte[] bytes;
        try { bytes = File.ReadAllBytes(Path.Combine(revision.Path, "data", refs.File)); }
        catch (Exception e) when (e is IOException or UnauthorizedAccessException) { throw new SaveException(refs.File + ": " + e.Message); }
        return ReferencedSiblings(bytes, layout);
    }

    /// <summary>
    /// <see cref="ReferencedSiblings(LocalRevision, SaveLayout)"/> on the bytes of the layout's reference file already read (CR-S105: the
    /// pack reads the names from the very bytes it verified against the commit and sends).
    /// </summary>
    public static IReadOnlySet<string> ReferencedSiblings(byte[] bytes, SaveLayout layout)
    {
        ArgumentNullException.ThrowIfNull(bytes);
        ArgumentNullException.ThrowIfNull(layout);
        var names = new HashSet<string>(StringComparer.Ordinal);
        if (layout.References is not { } refs) return names;
        var temp = Path.Combine(Path.GetTempPath(), "fi-save-refs-" + Guid.NewGuid().ToString("N") + ".db");
        try
        {
            if (bytes.Length <= refs.HeaderBytes) throw new SaveException(refs.File + " is too short");
            File.WriteAllBytes(temp, bytes[refs.HeaderBytes..]);
            using var db = new Microsoft.Data.Sqlite.SqliteConnection(new Microsoft.Data.Sqlite.SqliteConnectionStringBuilder
            { DataSource = temp, Mode = Microsoft.Data.Sqlite.SqliteOpenMode.ReadOnly, Pooling = false }.ToString());
            db.Open();
            foreach (var column in refs.Columns)
            {
                using var query = db.CreateCommand();
                query.CommandText = $"SELECT \"{column.Replace("\"", "\"\"", StringComparison.Ordinal)}\" FROM \"{refs.Table.Replace("\"", "\"\"", StringComparison.Ordinal)}\"";
                using var reader = query.ExecuteReader();
                while (reader.Read())
                {
                    if (reader.IsDBNull(0) || reader.GetValue(0) is not string value) continue;
                    foreach (var prefix in refs.StripPrefixes)
                        if (value.StartsWith(prefix, StringComparison.Ordinal)) { value = value[prefix.Length..]; break; }
                    if (layout.IsSibling(value)) names.Add(value);
                }
            }
            return names;
        }
        catch (Microsoft.Data.Sqlite.SqliteException e) { throw new SaveException(refs.File + " is not a readable database: " + e.Message); }
        finally
        {
            try { if (File.Exists(temp)) File.Delete(temp); }
            catch (Exception e) when (e is IOException or UnauthorizedAccessException) { }
        }
    }
}
