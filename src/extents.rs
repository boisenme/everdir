//! Reflinked files (`cp` clones on XFS/btrfs, copies made by coreutils 9 by default, snapshots)
//! share physical extents, yet `st_blocks` reports the full size for every copy. Shared extents
//! are read with FIEMAP and charged only to the first file seen holding them.

#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::collections::BTreeMap;
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::Mutex;

/// Filesystems that can share extents between files.
pub const REFLINK_FS: [&str; 3] = ["xfs", "btrfs", "bcachefs"];

const FS_IOC_FIEMAP: u64 = 0xC020_660B;
const LAST: u32 = 0x1;
const UNKNOWN: u32 = 0x2;
const DELALLOC: u32 = 0x4;
const INLINE: u32 = 0x200;
const SHARED: u32 = 0x2000;
const BATCH: usize = 64;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Extent {
    logical: u64,
    physical: u64,
    length: u64,
    _r64: [u64; 2],
    flags: u32,
    _r32: [u32; 3],
}

#[repr(C)]
struct Fiemap {
    start: u64,
    length: u64,
    flags: u32,
    mapped: u32,
    count: u32,
    _r: u32,
    ext: [Extent; BATCH],
}

/// Physical byte ranges already charged to some file: disjoint, keyed by start, value is end.
#[derive(Default)]
pub struct Claimed(Mutex<BTreeMap<u64, u64>>);

impl Claimed {
    /// Bytes of `path`'s data that an earlier file was already charged for.
    pub fn charged_before(&self, path: &Path) -> u64 {
        let shared = shared_extents(path);
        if shared.is_empty() {
            return 0;
        }
        let mut m = self.0.lock().unwrap_or_else(|e| e.into_inner());
        shared.into_iter().map(|(s, e)| (e - s) - claim(&mut m, s, e)).sum()
    }
}

/// Marks [s, e) as charged and returns how many of its bytes were not charged before.
fn claim(m: &mut BTreeMap<u64, u64>, s: u64, e: u64) -> u64 {
    let (mut lo, mut hi, mut seen) = (s, e, 0);
    let from = m.range(..=s).next_back().filter(|&(_, &end)| end >= s).map_or(s, |(&a, _)| a);
    let over: Vec<(u64, u64)> = m.range(from..=e).map(|(&a, &b)| (a, b)).collect();
    for (a, b) in over {
        seen += b.min(e).saturating_sub(a.max(s));
        (lo, hi) = (lo.min(a), hi.max(b));
        m.remove(&a);
    }
    m.insert(lo, hi);
    (e - s) - seen
}

/// Physical ranges of the file's extents that are shared with other files.
#[cfg(not(target_os = "linux"))]
fn shared_extents(_path: &Path) -> Vec<(u64, u64)> {
    Vec::new()
}

/// Physical ranges of the file's extents that are shared with other files.
#[cfg(target_os = "linux")]
fn shared_extents(path: &Path) -> Vec<(u64, u64)> {
    let mut out = Vec::new();
    let Ok(f) = File::open(path) else { return out };
    let mut fm = Fiemap { start: 0, length: u64::MAX, flags: 0, mapped: 0, count: BATCH as u32, _r: 0, ext: [Extent::default(); BATCH] };
    loop {
        fm.length = u64::MAX - fm.start;
        fm.mapped = 0;
        // SAFETY: `fm` is a correctly laid out struct fiemap with room for `count` extents.
        if unsafe { libc::ioctl(f.as_raw_fd(), FS_IOC_FIEMAP as _, &mut fm) } != 0 {
            return out;
        }
        let n = (fm.mapped as usize).min(BATCH);
        for e in &fm.ext[..n] {
            if e.flags & SHARED != 0 && e.flags & (UNKNOWN | DELALLOC | INLINE) == 0 && e.length > 0 {
                out.push((e.physical, e.physical.saturating_add(e.length)));
            }
        }
        let Some(last) = fm.ext[..n].last() else { return out };
        if last.flags & LAST != 0 || n < BATCH {
            return out;
        }
        fm.start = last.logical + last.length;
    }
}

#[cfg(test)]
mod tests {
    use super::claim;
    use std::collections::BTreeMap;

    #[test]
    fn claims_each_byte_once() {
        let mut m = BTreeMap::new();
        assert_eq!(claim(&mut m, 100, 200), 100);
        assert_eq!(claim(&mut m, 100, 200), 0);
        assert_eq!(claim(&mut m, 150, 250), 50);
        assert_eq!(claim(&mut m, 0, 100), 100); // adjacent, merges
        assert_eq!(claim(&mut m, 300, 400), 100);
        assert_eq!(claim(&mut m, 0, 500), 150);
        assert_eq!(m.into_iter().collect::<Vec<_>>(), vec![(0, 500)]);
    }
}
