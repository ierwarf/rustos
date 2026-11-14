#![no_std]

//! Ring3/host-tool policy for bounded boot library prewarming.
//!
//! This plan is a performance hint, not file, frame, or mapping authority.
//! The pager must independently acquire an immutable provider capability and
//! verify each payload before publishing cache pages. Cold/large files remain
//! demand-paged; writable private views use COW rather than modifying the cache.

pub const MANIFEST_PATH: &str = "/etc/pagerd/library-cache.conf";
pub const MANIFEST_VERSION: &str = "rustos-library-cache-v1";
pub const PAGE_BYTES: u64 = rustos_user_abi::syscall::FRAME_SOURCE_PAGE_BYTES;
pub const MAX_MANIFEST_BYTES: usize = 65_536;
pub const MAX_PATH_BYTES: usize = 512;
pub const MAX_WARM_ENTRIES: u16 = 128;
// A valid boot plan must fit the supplied-page capability envelope. The
// default deliberately leaves at least half of it available to lazy sources.
pub const MAX_BUDGET_BYTES: u64 =
    rustos_user_abi::syscall::FRAME_SOURCE_MAX_LIVE as u64 * PAGE_BYTES;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Policy {
    pub budget_bytes: u64,
    pub max_file_bytes: u64,
    pub max_entries: u16,
    pub min_applications: u32,
    pub read_ahead_pages: u16,
}

impl Policy {
    pub const DEFAULT: Self = Self {
        budget_bytes: 32 * 1024 * 1024,
        max_file_bytes: 4 * 1024 * 1024,
        max_entries: MAX_WARM_ENTRIES,
        min_applications: 2,
        read_ahead_pages: 16,
    };

    pub const fn is_valid(self) -> bool {
        self.budget_bytes > 0
            && self.budget_bytes <= MAX_BUDGET_BYTES
            && self.max_file_bytes > 0
            && self.max_file_bytes <= self.budget_bytes
            && self.max_entries > 0
            && self.max_entries <= MAX_WARM_ENTRIES
            && self.min_applications > 0
            && self.read_ahead_pages > 0
            && self.read_ahead_pages <= 16
    }
}

/// Deliberate initial policy for ubiquitous ABI runtimes, not a claim that a
/// static dependency count measures runtime heat. Everything else must have
/// multiple distinct application consumers to qualify for boot prewarming.
pub fn is_common_runtime(name: &str) -> bool {
    matches!(
        name,
        "libc.so.6"
            | "libm.so.6"
            | "libdl.so.2"
            | "libpthread.so.0"
            | "librt.so.1"
            | "libgcc_s.so.1"
            | "libstdc++.so.6"
    )
}

pub fn resident_bytes(bytes: u64) -> Option<u64> {
    bytes
        .checked_add(PAGE_BYTES - 1)
        .map(|n| n & !(PAGE_BYTES - 1))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LazyReason {
    Empty,
    Large,
    Rare,
    Budget,
    EntryLimit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    Warm { resident_bytes: u64 },
    Lazy(LazyReason),
}

pub struct Selection {
    policy: Policy,
    resident_bytes: u64,
    entries: u16,
}

impl Selection {
    pub fn new(policy: Policy) -> Result<Self, Error> {
        if !policy.is_valid() {
            return Err(Error::Policy);
        }
        Ok(Self {
            policy,
            resident_bytes: 0,
            entries: 0,
        })
    }

    /// Call in priority order. Rejection consumes no budget, allowing smaller
    /// later candidates to fit. The budget accounts for complete resident pages.
    pub fn consider(&mut self, bytes: u64, applications: u32, common: bool) -> Admission {
        if bytes == 0 {
            return Admission::Lazy(LazyReason::Empty);
        }
        if bytes > self.policy.max_file_bytes {
            return Admission::Lazy(LazyReason::Large);
        }
        if !common && applications < self.policy.min_applications {
            return Admission::Lazy(LazyReason::Rare);
        }
        if self.entries == self.policy.max_entries {
            return Admission::Lazy(LazyReason::EntryLimit);
        }
        let Some(pages) = resident_bytes(bytes) else {
            return Admission::Lazy(LazyReason::Large);
        };
        let Some(total) = self.resident_bytes.checked_add(pages) else {
            return Admission::Lazy(LazyReason::Budget);
        };
        if total > self.policy.budget_bytes {
            return Admission::Lazy(LazyReason::Budget);
        }
        self.entries += 1;
        self.resident_bytes = total;
        Admission::Warm {
            resident_bytes: pages,
        }
    }

    pub const fn selected_bytes(&self) -> u64 {
        self.resident_bytes
    }
    pub const fn selected_entries(&self) -> u16 {
        self.entries
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Length,
    Header,
    Policy,
    Entry,
    Path,
    Digest,
    Duplicate,
    Budget,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry<'a> {
    pub path: &'a str,
    pub bytes: u64,
    pub sha256: [u8; 32],
}

/// Validated borrowed plan: no allocation and bounded duplicate checking.
/// Text format: version TAB budget TAB max-file TAB max-entries TAB min-apps
/// TAB read-ahead, followed by rows of bytes TAB lowercase-SHA256 TAB path.
#[derive(Debug)]
pub struct Plan<'a> {
    pub policy: Policy,
    rows: &'a str,
    count: u16,
    resident_bytes: u64,
}

impl<'a> Plan<'a> {
    pub fn parse(text: &'a str) -> Result<Self, Error> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(Error::Length);
        }
        if !text.ends_with('\n') || text.contains('\r') {
            return Err(Error::Entry);
        }
        let (header, rows) = text.split_once('\n').ok_or(Error::Header)?;
        let mut fields = header.split('\t');
        if fields.next() != Some(MANIFEST_VERSION) {
            return Err(Error::Header);
        }
        let policy = Policy {
            budget_bytes: decimal(fields.next().ok_or(Error::Header)?)?,
            max_file_bytes: decimal(fields.next().ok_or(Error::Header)?)?,
            max_entries: decimal(fields.next().ok_or(Error::Header)?)?
                .try_into()
                .map_err(|_| Error::Policy)?,
            min_applications: decimal(fields.next().ok_or(Error::Header)?)?
                .try_into()
                .map_err(|_| Error::Policy)?,
            read_ahead_pages: decimal(fields.next().ok_or(Error::Header)?)?
                .try_into()
                .map_err(|_| Error::Policy)?,
        };
        if fields.next().is_some() || !policy.is_valid() {
            return Err(Error::Policy);
        }
        let mut count: u16 = 0;
        let mut total: u64 = 0;
        let mut seen_paths = [""; MAX_WARM_ENTRIES as usize];
        for line in rows.lines() {
            let entry = parse_entry(line)?;
            if entry.bytes == 0 || entry.bytes > policy.max_file_bytes {
                return Err(Error::Entry);
            }
            if seen_paths[..count as usize].contains(&entry.path) {
                return Err(Error::Duplicate);
            }
            count = count.checked_add(1).ok_or(Error::Budget)?;
            if count > policy.max_entries {
                return Err(Error::Budget);
            }
            total = total
                .checked_add(resident_bytes(entry.bytes).ok_or(Error::Budget)?)
                .ok_or(Error::Budget)?;
            if total > policy.budget_bytes {
                return Err(Error::Budget);
            }
            seen_paths[(count - 1) as usize] = entry.path;
        }
        Ok(Self {
            policy,
            rows,
            count,
            resident_bytes: total,
        })
    }

    pub fn entries(&self) -> impl Iterator<Item = Result<Entry<'a>, Error>> + '_ {
        self.rows.lines().map(parse_entry)
    }
    pub const fn entry_count(&self) -> u16 {
        self.count
    }
    pub const fn resident_bytes(&self) -> u64 {
        self.resident_bytes
    }
}

pub fn valid_path(path: &str) -> bool {
    path.len() <= MAX_PATH_BYTES
        && path.starts_with('/')
        && path.len() > 1
        && !path.chars().any(char::is_control)
        && path[1..]
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn decimal(text: &str) -> Result<u64, Error> {
    if text.is_empty()
        || !text.bytes().all(|b| b.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return Err(Error::Entry);
    }
    text.parse().map_err(|_| Error::Entry)
}

fn parse_entry(line: &str) -> Result<Entry<'_>, Error> {
    let mut fields = line.split('\t');
    let bytes = decimal(fields.next().ok_or(Error::Entry)?)?;
    let digest = fields.next().ok_or(Error::Entry)?;
    let path = fields.next().ok_or(Error::Entry)?;
    if fields.next().is_some() {
        return Err(Error::Entry);
    }
    if !valid_path(path) {
        return Err(Error::Path);
    }
    if digest.len() != 64 {
        return Err(Error::Digest);
    }
    let mut sha256 = [0; 32];
    for (target, pair) in sha256.iter_mut().zip(digest.as_bytes().chunks_exact(2)) {
        let hex = |b| match b {
            b'0'..=b'9' => Ok(b - b'0'),
            b'a'..=b'f' => Ok(b - b'a' + 10),
            _ => Err(Error::Digest),
        };
        *target = hex(pair[0])? * 16 + hex(pair[1])?;
    }
    Ok(Entry {
        path,
        bytes,
        sha256,
    })
}

#[cfg(test)]
extern crate std;
#[cfg(test)]
mod tests;
