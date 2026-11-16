//! The SQLite file format, read-only and only as far as the reader needs it
//! (<https://www.sqlite.org/fileformat2.html>): the database header, table b-trees (interior 0x05
//! and leaf 0x0D pages), overflow chains and records. No index b-trees, no freelist, no WAL, no
//! pointer map: none of them is on the path from `sqlite_schema` to a table's rows.
//!
//! Every page is read at most once per [`File`] (a page reached twice is a cycle or two trees
//! sharing a page; either refuses), at most [`MAX_PAGES_READ`] in all; the bytes of records
//! assembled are bounded too ([`File`]'s byte budget: one leaf may point every cell at the same
//! record), and every offset is checked before it is used. A hostile file costs at most that much
//! work and never panics. Only the limits are public: the reader's types stay in the crate, so no
//! caller can reach a record's text bytes.

use crate::Unreadable;

/// The 16-byte magic every SQLite 3 database starts with.
pub const MAGIC: &[u8; 16] = b"SQLite format 3\0";
/// The largest database read (the game's PlayerDatabase is 6,291,456 bytes inside its container).
pub const MAX_DATABASE: usize = 8 << 20;
/// The most pages one read visits, overflow pages included: a garage of 550 cars with every part
/// and tuning value set fills about 1,200 of the game's 1 KiB pages.
pub const MAX_PAGES_READ: u32 = 4_096;
/// The deepest table b-tree followed (a 1 KiB-page tree of 23,650 rows is 3 levels deep).
pub const MAX_DEPTH: usize = 12;
/// The largest record assembled (the longest schema row, `Career_Garage`'s DDL, is 2.8 KB).
pub const MAX_PAYLOAD: usize = 64 << 10;
/// The most columns a record may hold (`Career_Garage` has 124).
pub const MAX_COLUMNS: usize = 256;
/// The most record bytes one read assembles, as a multiple of the database's length (each row of
/// a well-formed file is assembled once, so 1 would do; the margin is for the schema's rows).
pub const MAX_ASSEMBLED_PER_BYTE: usize = 2;

/// One database, opened for reading. Owns the visited-page set and the page budget.
pub(crate) struct File<'a> {
    bytes: &'a [u8],
    page_size: usize,
    usable: usize,
    pages: u32,
    visited: Vec<bool>,
    read: u32,
    assembled: usize,
}

impl<'a> File<'a> {
    /// Checks the header. The page count is the file length over the page size: the header's own
    /// count is not trusted (the game's files carry 0 there, a pre-3.7.0 writer's mark).
    pub(crate) fn open(bytes: &'a [u8]) -> Result<Self, Unreadable> {
        if bytes.len() > MAX_DATABASE {
            return Err(Unreadable::TooLarge);
        }
        if bytes.len() < 512 || bytes.get(..16) != Some(MAGIC.as_slice()) {
            return Err(Unreadable::Header("magic"));
        }
        let page_size = match u16::from_be_bytes([bytes[16], bytes[17]]) {
            1 => 65_536,
            n if n >= 512 && n.is_power_of_two() => usize::from(n),
            _ => return Err(Unreadable::Header("page size")),
        };
        // Rollback journal only: in WAL mode (2) the newest pages may live in a file we never get.
        if bytes[18] != 1 || bytes[19] != 1 {
            return Err(Unreadable::Header("file format"));
        }
        let usable = page_size - usize::from(bytes[20]);
        if usable < 480 {
            return Err(Unreadable::Header("reserved space"));
        }
        if bytes[21..24] != [64, 32, 32] {
            return Err(Unreadable::Header("payload fractions"));
        }
        if !(1..=4).contains(&be32(bytes, 44)) {
            return Err(Unreadable::Header("schema format"));
        }
        if be32(bytes, 56) != 1 {
            return Err(Unreadable::Header("text encoding"));
        }
        if !bytes.len().is_multiple_of(page_size) {
            return Err(Unreadable::Header("length"));
        }
        let pages = (bytes.len() / page_size) as u32;
        Ok(File {
            bytes,
            page_size,
            usable,
            pages,
            visited: vec![false; pages as usize + 1],
            read: 0,
            assembled: 0,
        })
    }

    /// Pages read so far.
    pub(crate) fn pages_read(&self) -> u32 {
        self.read
    }

    /// Page `n` (from 1), its usable part only. Each page once, within the budget.
    fn page(&mut self, n: u32) -> Result<&'a [u8], Unreadable> {
        if n == 0 || n > self.pages {
            return Err(Unreadable::Page(n));
        }
        if std::mem::replace(&mut self.visited[n as usize], true) {
            return Err(Unreadable::PageReused(n));
        }
        self.read += 1;
        if self.read > MAX_PAGES_READ {
            return Err(Unreadable::Budget);
        }
        let start = (n as usize - 1) * self.page_size;
        Ok(&self.bytes[start..start + self.usable])
    }

    /// Calls `row(rowid, payload)` for every row of the table b-tree rooted at `root`, in rowid
    /// order. `payload` is the whole record, overflow included.
    pub(crate) fn scan(
        &mut self,
        root: u32,
        mut row: impl FnMut(i64, &[u8]) -> Result<(), Unreadable>,
    ) -> Result<(), Unreadable> {
        let mut payload = Vec::new();
        let mut offsets = Vec::new();
        // Pages still to read, the next one last; the depth bounds the stack.
        let mut stack = vec![(root, 0usize)];
        while let Some((n, depth)) = stack.pop() {
            if depth > MAX_DEPTH {
                return Err(Unreadable::Depth);
            }
            let page = self.page(n)?;
            let h = if n == 1 { 100 } else { 0 };
            let kind = *page.get(h).ok_or(Unreadable::Page(n))?;
            let cells = usize::from(be16(page, h + 3).ok_or(Unreadable::Page(n))?);
            let pointers = h + if kind == 0x05 { 12 } else { 8 };
            if pointers + 2 * cells > page.len() {
                return Err(Unreadable::Page(n));
            }
            let cell = |i: usize| -> Result<usize, Unreadable> {
                let at = usize::from(be16(page, pointers + 2 * i).ok_or(Unreadable::Page(n))?);
                if at < pointers + 2 * cells || at >= page.len() {
                    return Err(Unreadable::Page(n));
                }
                Ok(at)
            };
            // Every cell its own: SQLite never points two cells at the same bytes, and a hostile
            // leaf that does would replay one record many times for one page of budget.
            offsets.clear();
            for i in 0..cells {
                offsets.push(cell(i)?);
            }
            offsets.sort_unstable();
            if offsets.windows(2).any(|w| w[0] == w[1]) {
                return Err(Unreadable::Page(n));
            }
            match kind {
                0x05 => {
                    let right = be32_at(page, h + 8).ok_or(Unreadable::Page(n))?;
                    stack.push((right, depth + 1));
                    for i in (0..cells).rev() {
                        let child = be32_at(page, cell(i)?).ok_or(Unreadable::Page(n))?;
                        stack.push((child, depth + 1));
                    }
                }
                0x0D => {
                    for i in 0..cells {
                        let at = cell(i)?;
                        let (size, a) = varint(page, at).ok_or(Unreadable::Page(n))?;
                        let (rowid, b) = varint(page, at + a).ok_or(Unreadable::Page(n))?;
                        let size = usize::try_from(size)
                            .ok()
                            .filter(|s| *s <= MAX_PAYLOAD)
                            .ok_or(Unreadable::Payload)?;
                        self.assembled += size;
                        if self.assembled > MAX_ASSEMBLED_PER_BYTE * self.bytes.len() {
                            return Err(Unreadable::Budget);
                        }
                        self.payload(page, n, at + a + b, size, &mut payload)?;
                        row(rowid as i64, &payload)?;
                    }
                }
                _ => return Err(Unreadable::Page(n)),
            }
        }
        Ok(())
    }

    /// Assembles one leaf cell's payload of `size` bytes starting at `at` into `out`.
    fn payload(
        &mut self,
        page: &[u8],
        n: u32,
        at: usize,
        size: usize,
        out: &mut Vec<u8>,
    ) -> Result<(), Unreadable> {
        let u = self.usable;
        let x = u - 35;
        let local = if size <= x {
            size
        } else {
            let m = (u - 12) * 32 / 255 - 23;
            let k = m + (size - m) % (u - 4);
            if k <= x { k } else { m }
        };
        out.clear();
        out.extend_from_slice(page.get(at..at + local).ok_or(Unreadable::Page(n))?);
        if local == size {
            return Ok(());
        }
        let mut next = be32_at(page, at + local).ok_or(Unreadable::Page(n))?;
        while out.len() < size {
            if next == 0 {
                return Err(Unreadable::Record(n));
            }
            let overflow = self.page(next)?;
            let take = (size - out.len()).min(u - 4);
            out.extend_from_slice(&overflow[4..4 + take]);
            next = be32_at(overflow, 0).ok_or(Unreadable::Page(next))?;
        }
        Ok(())
    }
}

/// One value's place in a record: its serial type and where its bytes start.
#[derive(Clone, Copy, Debug)]
struct Column {
    serial: u64,
    start: usize,
}

/// A decoded record header: the columns of one row, with their bytes in `payload`.
pub(crate) struct Record<'p> {
    payload: &'p [u8],
    columns: Vec<Column>,
}

impl<'p> Record<'p> {
    /// Parses the record header and checks every column's bytes lie inside the payload.
    pub(crate) fn parse(payload: &'p [u8]) -> Result<Self, Unreadable> {
        let (header, mut at) = varint(payload, 0).ok_or(Unreadable::Payload)?;
        let header = usize::try_from(header)
            .ok()
            .filter(|h| *h >= at && *h <= payload.len())
            .ok_or(Unreadable::Payload)?;
        let mut start = header;
        let mut columns = Vec::new();
        while at < header {
            let (serial, used) = varint(&payload[..header], at).ok_or(Unreadable::Payload)?;
            at += used;
            let len = serial_len(serial).ok_or(Unreadable::Payload)?;
            if columns.len() == MAX_COLUMNS || len > payload.len() - start {
                return Err(Unreadable::Payload);
            }
            columns.push(Column { serial, start });
            start += len;
        }
        Ok(Record { payload, columns })
    }

    /// Column `i` as an integer. Absent (a row written before the column existed) or NULL is
    /// `None`; a REAL, TEXT or BLOB is an error, so the caller never sees those bytes.
    pub(crate) fn int(&self, i: usize) -> Result<Option<i64>, ()> {
        let Some(c) = self.columns.get(i) else {
            return Ok(None);
        };
        let p = &self.payload[c.start..];
        let be = |n: usize| -> i64 {
            let mut v: i64 = if p[0] & 0x80 != 0 { -1 } else { 0 };
            for b in &p[..n] {
                v = (v << 8) | i64::from(*b);
            }
            v
        };
        Ok(match c.serial {
            0 => None,
            1..=4 => Some(be(c.serial as usize)),
            5 => Some(be(6)),
            6 => Some(be(8)),
            8 => Some(0),
            9 => Some(1),
            _ => return Err(()),
        })
    }

    /// Column `i` as UTF-8 text; only the schema's own columns are ever read this way.
    pub(crate) fn text(&self, i: usize) -> Option<&'p str> {
        let c = self.columns.get(i)?;
        if c.serial < 13 || c.serial % 2 == 0 {
            return None;
        }
        let len = serial_len(c.serial)?;
        std::str::from_utf8(&self.payload[c.start..c.start + len]).ok()
    }
}

/// Bytes a serial type's value takes; `None` for the reserved types 10 and 11.
fn serial_len(serial: u64) -> Option<usize> {
    Some(match serial {
        0 | 8 | 9 => 0,
        1..=4 => serial as usize,
        5 => 6,
        6 | 7 => 8,
        10 | 11 => return None,
        n => usize::try_from((n - 12) / 2).ok()?,
    })
}

/// SQLite's big-endian varint (1-9 bytes): the value and the bytes it took.
pub(crate) fn varint(b: &[u8], at: usize) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..9 {
        let byte = *b.get(at + i)?;
        if i == 8 {
            return Some(((v << 8) | u64::from(byte), 9));
        }
        v = (v << 7) | u64::from(byte & 0x7f);
        if byte & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn be32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// A header field the caller already bounds-checked (the header is 100 bytes, the file ≥ 512).
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_of_every_length() {
        assert_eq!(varint(&[0x05], 0), Some((5, 1)));
        assert_eq!(varint(&[0x81, 0x00], 0), Some((128, 2)));
        assert_eq!(varint(&[0xff; 9], 0), Some((u64::MAX, 9)));
        assert_eq!(varint(&[0x81], 0), None);
    }

    #[test]
    fn serial_lengths() {
        assert_eq!(serial_len(7), Some(8));
        assert_eq!(serial_len(10), None);
        assert_eq!(serial_len(12), Some(0));
        assert_eq!(serial_len(13), Some(0));
        assert_eq!(serial_len(19), Some(3));
    }

    #[test]
    fn records_decode_ints_and_refuse_other_types() {
        // header 6 bytes: NULL, 1-byte int, 2-byte int, REAL, text of 2.
        let payload = [
            6, 0, 1, 2, 7, 17, 0xfe, 0x01, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, b'h', b'i',
        ];
        let r = Record::parse(&payload).unwrap();
        assert_eq!(r.int(0), Ok(None));
        assert_eq!(r.int(1), Ok(Some(-2)));
        assert_eq!(r.int(2), Ok(Some(256)));
        assert_eq!(r.int(3), Err(()));
        assert_eq!(r.int(4), Err(()));
        assert_eq!(r.int(9), Ok(None));
        assert_eq!(r.text(4), Some("hi"));
        // A column whose bytes run past the payload.
        assert!(Record::parse(&[2, 6, 0]).is_err());
    }
}
