//! The layout of iRacing's telemetry, as its SDK's `irsdk_defines.h` defines it: a header, the
//! description of each variable (its type, offset and count), and buffers holding one update each.
//! The shared memory the game writes while it runs and the `.ibt` files it saves both start with
//! it. Everything here reads copies of those bytes, so it never touches the game's memory itself.
//!
//! Values are little-endian (the game runs on x86 Windows). Variables are looked up by name: the
//! SDK says which ones there are by listing them, and a car or session may have fewer.

use std::collections::HashMap;

/// The shared memory's name (`IRSDK_MEMMAPFILENAME`).
pub const MEMORY_NAME: &str = "Local\\IRSDKMemMapFileName";

/// The header's version this reads (`IRSDK_VER`).
pub const VERSION: i32 = 2;
/// `irsdk_stConnected`, in the header's status: the game is in a session, writing it.
const STATUS_CONNECTED: i32 = 1;
/// The most update buffers the game rotates through (`IRSDK_MAX_BUFS`).
const MAX_BUFS: usize = 4;
/// `irsdk_header`: ten ints, two of padding, then `MAX_BUFS` `irsdk_varBuf`s (four ints each).
pub const HEADER_LEN: usize = 12 * 4 + MAX_BUFS * 16;
/// `irsdk_varHeader`: four ints (the last one a bool and padding), then its name (`IRSDK_MAX_STRING`
/// bytes), description (`IRSDK_MAX_DESC`) and unit (`IRSDK_MAX_STRING`).
const VAR_HEADER_LEN: usize = 16 + 32 + 64 + 32;
const MAX_STRING: usize = 32;

/// A session or lap count that has no limit (`IRSDK_UNLIMITED_LAPS`).
pub const UNLIMITED_LAPS: i32 = 32767;
/// A session time that has no limit (`IRSDK_UNLIMITED_TIME`, seconds).
pub const UNLIMITED_TIME: f64 = 604800.0;

/// The header, at the start of the memory or file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: i32,
    pub status: i32,
    /// Updates a second
    pub tick_rate: i32,
    /// Incremented whenever the session info changes
    pub session_info_update: i32,
    pub session_info_len: i32,
    pub session_info_offset: i32,
    pub var_count: i32,
    pub var_header_offset: i32,
    pub buf_count: i32,
    /// The length of one update
    pub buf_len: i32,
    pub bufs: [Buf; MAX_BUFS],
}

/// One of the buffers the game writes its updates into, in turn (`irsdk_varBuf`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Buf {
    /// Which update it holds: set once the game has written it
    pub tick: i32,
    pub offset: i32,
    /// Set before the game starts writing an update into it, in SDK versions that have it (the
    /// int after the offset, which older ones have as padding; pyirsdk, which follows the SDK's
    /// header, names it `tick_count_begin`, "updated BEFORE write starts")
    pub tick_begin: i32,
}

fn int(bytes: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

impl Header {
    /// The header at the start of `bytes`, if they're long enough to hold one.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let at = |i: usize| int(bytes, i * 4);
        let mut bufs = [Buf::default(); MAX_BUFS];
        for (i, buf) in bufs.iter_mut().enumerate() {
            *buf = Buf {
                tick: at(12 + i * 4)?,
                offset: at(13 + i * 4)?,
                tick_begin: at(14 + i * 4)?,
            };
        }
        Some(Self {
            version: at(0)?,
            status: at(1)?,
            tick_rate: at(2)?,
            session_info_update: at(3)?,
            session_info_len: at(4)?,
            session_info_offset: at(5)?,
            var_count: at(6)?,
            var_header_offset: at(7)?,
            buf_count: at(8)?,
            buf_len: at(9)?,
            bufs,
        })
    }

    /// Whether the game is in a session, writing it.
    pub fn connected(&self) -> bool {
        self.status & STATUS_CONNECTED != 0
    }

    /// The buffer holding the latest update: its tick count and offset. None before the game
    /// has written one (no tick yet, or no place for it past the header).
    pub fn latest(&self) -> Option<(i32, usize)> {
        let count = usize::try_from(self.buf_count).ok()?.min(MAX_BUFS);
        let buf = self.bufs[..count].iter().max_by_key(|buf| buf.tick)?;
        let offset = usize::try_from(buf.offset)
            .ok()
            .filter(|o| *o >= HEADER_LEN)?;
        (buf.tick > 0).then_some((buf.tick, offset))
    }

    /// Whether the buffer at `offset` still holds update `tick`, and when the game says when it
    /// starts writing one (see `begins`), written whole: it hasn't started writing another into it.
    pub fn holds(&self, tick: i32, offset: usize) -> bool {
        let begins = self.begins();
        self.bufs.iter().any(|buf| {
            buf.tick == tick
                && usize::try_from(buf.offset) == Ok(offset)
                && (!begins || buf.tick_begin == tick)
        })
    }

    /// Whether the start counts are counts: every buffer written to has one at its tick count, or
    /// a few updates past it while it's being written. Older SDK versions have padding there
    /// instead, which then reads as no count at all.
    fn begins(&self) -> bool {
        let count = usize::try_from(self.buf_count).unwrap_or(0).min(MAX_BUFS);
        let written: Vec<_> = self.bufs[..count]
            .iter()
            .filter(|buf| buf.tick > 0)
            .collect();
        !written.is_empty()
            && written.iter().all(|buf| {
                buf.tick_begin >= buf.tick && buf.tick_begin - buf.tick <= 2 * MAX_BUFS as i32
            })
    }

    /// Where the session info is, as a range of the bytes.
    pub fn session_info(&self) -> Option<std::ops::Range<usize>> {
        let start = usize::try_from(self.session_info_offset).ok()?;
        let len = usize::try_from(self.session_info_len).ok()?;
        Some(start..start.checked_add(len)?)
    }

    /// Where the variables' descriptions are, as a range of the bytes.
    pub fn var_headers(&self) -> Option<std::ops::Range<usize>> {
        let start = usize::try_from(self.var_header_offset).ok()?;
        let len = usize::try_from(self.var_count)
            .ok()?
            .checked_mul(VAR_HEADER_LEN)?;
        Some(start..start.checked_add(len)?)
    }

    /// The length of one update.
    pub fn buf_len(&self) -> Option<usize> {
        usize::try_from(self.buf_len).ok().filter(|len| *len > 0)
    }
}

/// A variable's type (`irsdk_VarType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Char,
    Bool,
    Int,
    BitField,
    Float,
    Double,
}

impl Kind {
    fn new(kind: i32) -> Option<Self> {
        Some(match kind {
            0 => Self::Char,
            1 => Self::Bool,
            2 => Self::Int,
            3 => Self::BitField,
            4 => Self::Float,
            5 => Self::Double,
            _ => return None,
        })
    }

    /// `irsdk_VarTypeBytes`
    fn size(self) -> usize {
        match self {
            Self::Char | Self::Bool => 1,
            Self::Int | Self::BitField | Self::Float => 4,
            Self::Double => 8,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Var {
    kind: Kind,
    offset: usize,
    count: usize,
}

/// The variables an update has, by name.
#[derive(Debug, Default)]
pub struct Vars(HashMap<String, Var>);

impl Vars {
    /// Doubles named `names`, one after another, for tests: the variables and an update of
    /// `values` in them.
    #[cfg(test)]
    pub fn doubles(values: &[(&str, f64)]) -> (Self, Vec<u8>) {
        let mut vars = HashMap::new();
        let mut data = Vec::new();
        for (name, value) in values {
            vars.insert(
                (*name).to_owned(),
                Var {
                    kind: Kind::Double,
                    offset: data.len(),
                    count: 1,
                },
            );
            data.extend_from_slice(&value.to_le_bytes());
        }
        (Self(vars), data)
    }

    /// The variables `header` describes in `bytes` (the memory or file it's the header of). Those
    /// that don't fit in an update are left out.
    pub fn parse(bytes: &[u8], header: &Header) -> Option<Self> {
        Some(Self::from_headers(
            bytes.get(header.var_headers()?)?,
            header.buf_len()?,
        ))
    }

    /// The variables described in `headers` (`irsdk_varHeader`s one after another), in updates
    /// `buf_len` long.
    pub fn from_headers(headers: &[u8], buf_len: usize) -> Self {
        let mut vars = HashMap::new();
        for desc in headers.chunks_exact(VAR_HEADER_LEN) {
            let (Some(kind), Some(offset), Some(count)) = (
                int(desc, 0).and_then(Kind::new),
                int(desc, 4).and_then(|offset| usize::try_from(offset).ok()),
                int(desc, 8).and_then(|count| usize::try_from(count).ok()),
            ) else {
                continue;
            };
            let name = &desc[16..16 + MAX_STRING];
            let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
            let Ok(name) = std::str::from_utf8(&name[..end]) else {
                continue;
            };
            let fits = count
                .checked_mul(kind.size())
                .and_then(|len| offset.checked_add(len))
                .is_some_and(|end| end <= buf_len);
            if fits && count > 0 {
                vars.insert(
                    name.to_owned(),
                    Var {
                        kind,
                        offset,
                        count,
                    },
                );
            }
        }
        Self(vars)
    }
}

/// One update: its bytes, read with the variables' descriptions.
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    vars: &'a Vars,
    data: &'a [u8],
}

impl<'a> Frame<'a> {
    pub fn new(vars: &'a Vars, data: &'a [u8]) -> Self {
        Self { vars, data }
    }

    fn bytes(&self, name: &str, index: usize) -> Option<(Kind, &'a [u8])> {
        let var = self.vars.0.get(name)?;
        if index >= var.count {
            return None;
        }
        let at = var.offset + index * var.kind.size();
        Some((var.kind, self.data.get(at..at + var.kind.size())?))
    }

    /// How many values the variable has: 0 when there's no such variable.
    pub fn count(&self, name: &str) -> usize {
        self.vars.0.get(name).map_or(0, |var| var.count)
    }

    /// The `index`th value of a float or double, if it's a finite number.
    pub fn f64_at(&self, name: &str, index: usize) -> Option<f64> {
        let value = match self.bytes(name, index)? {
            (Kind::Float, b) => f32::from_le_bytes(b.try_into().ok()?) as f64,
            (Kind::Double, b) => f64::from_le_bytes(b.try_into().ok()?),
            _ => return None,
        };
        value.is_finite().then_some(value)
    }

    pub fn f64(&self, name: &str) -> Option<f64> {
        self.f64_at(name, 0)
    }

    /// The `index`th value of an int or bit field (bit fields as their bits).
    pub fn i32_at(&self, name: &str, index: usize) -> Option<i32> {
        match self.bytes(name, index)? {
            (Kind::Int | Kind::BitField, b) => Some(i32::from_le_bytes(b.try_into().ok()?)),
            _ => None,
        }
    }

    pub fn i32(&self, name: &str) -> Option<i32> {
        self.i32_at(name, 0)
    }

    /// The bits of a bit field (or an int).
    pub fn bits(&self, name: &str) -> Option<u32> {
        self.i32(name).map(|bits| bits as u32)
    }

    pub fn bits_at(&self, name: &str, index: usize) -> Option<u32> {
        self.i32_at(name, index).map(|bits| bits as u32)
    }

    /// The `index`th value of a bool.
    pub fn bool_at(&self, name: &str, index: usize) -> Option<bool> {
        match self.bytes(name, index)? {
            (Kind::Bool, b) => Some(b[0] != 0),
            _ => None,
        }
    }

    pub fn bool(&self, name: &str) -> Option<bool> {
        self.bool_at(name, 0)
    }
}

/// A telemetry file the game saved (`.ibt`): the header, `irsdk_diskSubHeader`, then every update
/// one after another in the first buffer. Only read by tests, to check the converter against what
/// the game recorded.
#[cfg(test)]
pub struct Ibt {
    data: Vec<u8>,
    vars: Vars,
    first: usize,
    len: usize,
    records: usize,
    session_info: std::ops::Range<usize>,
    tick_rate: i32,
}

#[cfg(test)]
impl Ibt {
    pub fn open(path: &std::path::Path) -> std::io::Result<Self> {
        let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "not an .ibt file");
        let data = std::fs::read(path)?;
        let header = Header::parse(&data).ok_or_else(invalid)?;
        let vars = Vars::parse(&data, &header).ok_or_else(invalid)?;
        // `irsdk_diskSubHeader`: a time_t, two doubles, the lap count, then the record count
        let records = int(&data, HEADER_LEN + 8 + 8 + 8 + 4)
            .and_then(|count| usize::try_from(count).ok())
            .ok_or_else(invalid)?;
        let first = usize::try_from(header.bufs[0].offset).map_err(|_| invalid())?;
        let len = header.buf_len().ok_or_else(invalid)?;
        let records = records.min(data.len().saturating_sub(first) / len);
        let session_info = header
            .session_info()
            .filter(|range| range.end <= data.len())
            .ok_or_else(invalid)?;
        Ok(Self {
            data,
            vars,
            first,
            len,
            records,
            session_info,
            tick_rate: header.tick_rate,
        })
    }

    pub fn records(&self) -> usize {
        self.records
    }

    /// Updates a second.
    pub fn tick_rate(&self) -> i32 {
        self.tick_rate
    }

    /// The session info the game saved with the file (as it was when it closed it).
    pub fn session_info(&self) -> &[u8] {
        &self.data[self.session_info.clone()]
    }

    pub fn frame(&self, index: usize) -> Frame<'_> {
        let at = self.first + index * self.len;
        Frame::new(&self.vars, &self.data[at..at + self.len])
    }
}
