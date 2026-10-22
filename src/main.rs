//! everdir — whole-disk file index with instant search (Everything-style) and a
//! cushion treemap / size tree (WinDirStat-style). Runs as a local daemon and
//! serves its UI on http://127.0.0.1:7171.

use memchr::memmem;
use rayon::prelude::*;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tiny_http::{Header, Method, Request, Response, Server};

mod drives;
mod extents;
use drives::{DriveCfg, Meta};

const FILE: u8 = 0;
const DIR: u8 = 1;
const LINK: u8 = 2;
const NONE: u32 = u32::MAX;
const DEFAULT_PORT: u16 = 7171;
/// Seconds before a drive that could not be read is tried again on schedule.
const RETRY_AFTER: u64 = 3600;
// Mount points whose subtrees are never indexed (pseudo filesystems and snapper snapshots).
#[cfg(not(target_os = "macos"))]
const SKIP: &[&str] = &["/proc", "/sys", "/dev", "/run", "/tmp", "/.snapshots"];
// On macOS the data volume is firmlinked into "/" (/Users, /Applications, ...), so the volumes
// under /System/Volumes would only be counted twice.
#[cfg(target_os = "macos")]
const SKIP: &[&str] = &["/dev", "/System/Volumes", "/private/var/vm", "/Volumes/com.apple.TimeMachine.localsnapshots"];
#[cfg(target_os = "macos")]
const LAUNCHD_LABEL: &str = "io.github.boisenme.everdir";
// WinDirStat's palette; colours are brightness-normalised at paint time (see `normalize`).
const PALETTE: [[u8; 3]; 16] = [
    [0, 0, 255], [255, 0, 0], [0, 255, 0], [0, 255, 255], [255, 0, 255], [255, 255, 0],
    [150, 150, 255], [255, 150, 150], [150, 255, 150], [150, 255, 255], [255, 150, 255], [255, 255, 150],
    [255, 128, 0], [128, 0, 255], [0, 160, 128], [190, 130, 70],
];
const OTHER_COL: [u8; 3] = [165, 165, 165];
static HTML: &str = include_str!("index.html");
static ICON: &str = include_str!("../dist/everdir.svg");

fn port() -> u16 {
    std::env::var("EVERDIR_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}
fn rd<T>(l: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(|e| e.into_inner())
}
fn wr<T>(l: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- scanning

struct Tmp {
    name: Box<str>,
    kind: u8,
    size: u64,
    alloc: u64,
    mtime: i64,
    files: u32,
    children: Vec<Tmp>,
}

struct Ctx {
    skip: HashSet<PathBuf>,
    seen: Mutex<HashSet<(u64, u64)>>,
    /// Set on filesystems with reflinks; tracks which shared extents were already counted.
    reflink: Option<extents::Claimed>,
    errors: AtomicU64,
}

fn scan_dir(path: &Path, ctx: &Ctx) -> Vec<Tmp> {
    let rd = match fs::read_dir(path) {
        Ok(r) => r,
        Err(_) => {
            ctx.errors.fetch_add(1, SeqCst);
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    let mut sub: Vec<(usize, PathBuf)> = Vec::new();
    for e in rd.flatten() {
        // DirEntry::metadata does not follow symlinks.
        let Ok(md) = e.metadata() else { continue };
        let ft = md.file_type();
        let kind = if ft.is_dir() {
            DIR
        } else if ft.is_file() {
            FILE
        } else if ft.is_symlink() {
            LINK
        } else {
            3
        };
        let (mut size, mut alloc) = (md.len(), md.blocks() * 512);
        if kind == DIR {
            (size, alloc) = (0, 0);
            let p = e.path();
            if !ctx.skip.contains(&p) {
                sub.push((out.len(), p));
            }
        } else if kind == FILE && md.nlink() > 1 && !ctx.seen.lock().unwrap().insert((md.dev(), md.ino())) {
            (size, alloc) = (0, 0); // count hardlinked data once
        }
        if kind == FILE && alloc > 0 && let Some(c) = &ctx.reflink {
            alloc = alloc.saturating_sub(c.charged_before(&e.path())); // count reflinked data once
        }
        out.push(Tmp {
            name: e.file_name().to_string_lossy().into(),
            kind,
            size,
            alloc,
            mtime: md.mtime(),
            files: (kind != DIR) as u32,
            children: Vec::new(),
        });
    }
    let kids: Vec<(usize, Vec<Tmp>)> = sub.into_par_iter().map(|(i, p)| (i, scan_dir(&p, ctx))).collect();
    for (i, ch) in kids {
        let t = &mut out[i];
        for c in &ch {
            t.size += c.size;
            t.alloc += c.alloc;
            t.files += c.files;
        }
        t.children = ch;
    }
    out
}

// ---------------------------------------------------------------- index

/// Flat struct-of-arrays tree. Children of a node are contiguous
/// (`first..first+nkids`) and sorted by allocated size, largest first.
#[derive(Default)]
struct Index {
    epoch: u64,
    /// Lowercased names, back to back; node i's is `lower[loff[i]..loff[i + 1]]`.
    lower: String,
    loff: Vec<u32>,
    /// Original spelling of the names that differ from their lowercase form (most don't), with
    /// (node, start in `orig`) sorted by node.
    orig: String,
    orig_at: Vec<(u32, u32)>,
    parent: Vec<u32>,
    first: Vec<u32>,
    nkids: Vec<u32>,
    size: Vec<u64>,
    alloc: Vec<u64>,
    files: Vec<u32>,
    mtime: Vec<i64>,
    kind: Vec<u8>,
    ext: Vec<u16>,
    dead: Vec<bool>,
    exts: Vec<String>,
    ext_ids: HashMap<String, u16>,
    /// Allocated bytes and file count per extension, for each drive.
    ext_stats: Vec<Vec<(u64, u32)>>,
    /// Drive mount points ("/" first), each one's node, and the drive every node lives on.
    drives: Vec<String>,
    dmount: Vec<u32>,
    drive: Vec<u8>,
    /// Folders only part of whose contents are indexed: those leading to a drive's mount point
    /// or chosen folders that no scan covers itself.
    partial: HashSet<u32>,
    dirs: u32,
    scanned_at: u64,
    scan_ms: u64,
    errors: u64,
}

fn ext_of(lname: &str) -> Option<&str> {
    let p = lname.rfind('.')?;
    let e = &lname[p + 1..];
    (p > 0 && e.len() <= 10 && !e.contains(' ') && e.bytes().any(|b| b.is_ascii_alphabetic())).then_some(e)
}

impl Index {
    fn build(root: Tmp, epoch: u64, scanned_at: u64, scan_ms: u64, errors: u64) -> Index {
        let mut ix = Index {
            epoch,
            loff: vec![0],
            exts: vec![String::new()],
            drives: vec!["/".into()],
            dmount: vec![0],
            scanned_at,
            scan_ms,
            errors,
            ..Default::default()
        };
        ix.ext_ids.insert(String::new(), 0);
        ix.push(&root, "/", NONE, 0);
        let mut q: VecDeque<(u32, Vec<Tmp>)> = VecDeque::new();
        q.push_back((0, root.children));
        while let Some((pid, mut kids)) = q.pop_front() {
            kids.sort_unstable_by(|a, b| b.alloc.cmp(&a.alloc).then(b.size.cmp(&a.size)));
            ix.first[pid as usize] = ix.parent.len() as u32;
            ix.nkids[pid as usize] = kids.len() as u32;
            for mut k in kids {
                let ch = std::mem::take(&mut k.children);
                let lname = k.name.to_lowercase();
                let e = match (k.kind, ext_of(&lname)) {
                    (FILE, Some(e)) => match ix.ext_ids.get(e) {
                        Some(&id) => id,
                        None => {
                            let id = ix.exts.len().min(u16::MAX as usize) as u16;
                            ix.exts.push(e.to_string());
                            ix.ext_ids.insert(e.to_string(), id);
                            id
                        }
                    },
                    _ => 0,
                };
                let id = ix.push(&k, &lname, pid, e);
                if !ch.is_empty() {
                    q.push_back((id, ch));
                }
            }
        }
        ix.shrink();
        ix.dirs = ix.kind.iter().filter(|&&k| k == DIR).count() as u32;
        ix.ext_stats = ix.compute_ext_stats();
        ix
    }

    fn push(&mut self, t: &Tmp, lname: &str, parent: u32, ext: u16) -> u32 {
        let id = self.parent.len() as u32;
        if *t.name != *lname {
            self.orig_at.push((id, self.orig.len() as u32));
            self.orig.push_str(&t.name);
        }
        self.lower.push_str(lname);
        self.loff.push(self.lower.len() as u32);
        self.parent.push(parent);
        self.first.push(0);
        self.nkids.push(0);
        self.size.push(t.size);
        self.alloc.push(t.alloc);
        self.files.push(t.files);
        self.mtime.push(t.mtime);
        self.kind.push(t.kind);
        self.ext.push(ext);
        self.dead.push(false);
        self.drive.push(0);
        id
    }

    /// Drops the spare capacity left over from growing the arrays during `build`.
    fn shrink(&mut self) {
        self.lower.shrink_to_fit();
        self.orig.shrink_to_fit();
        self.orig_at.shrink_to_fit();
        for v in [&mut self.loff, &mut self.parent, &mut self.first, &mut self.nkids, &mut self.files] {
            v.shrink_to_fit();
        }
        self.size.shrink_to_fit();
        self.alloc.shrink_to_fit();
        self.mtime.shrink_to_fit();
        self.kind.shrink_to_fit();
        self.ext.shrink_to_fit();
        self.dead.shrink_to_fit();
        self.drive.shrink_to_fit();
    }

    fn len(&self) -> usize {
        self.parent.len()
    }
    fn name(&self, i: u32) -> &str {
        match self.orig_at.binary_search_by_key(&i, |&(n, _)| n) {
            Ok(k) => {
                let end = self.orig_at.get(k + 1).map_or(self.orig.len(), |&(_, o)| o as usize);
                &self.orig[self.orig_at[k].1 as usize..end]
            }
            Err(_) => &self.lower[self.loff[i as usize] as usize..self.loff[i as usize + 1] as usize],
        }
    }
    fn lname(&self, i: u32) -> &[u8] {
        &self.lower.as_bytes()[self.loff[i as usize] as usize..self.loff[i as usize + 1] as usize]
    }
    fn kids(&self, i: u32) -> impl Iterator<Item = u32> + '_ {
        let f = self.first[i as usize];
        (f..f + self.nkids[i as usize]).filter(|&c| !self.dead[c as usize])
    }
    fn valid(&self, i: u32) -> bool {
        (i as usize) < self.len() && !self.dead[i as usize]
    }
    fn ancestors(&self, i: u32) -> Vec<u32> {
        let mut v = vec![];
        let mut c = i;
        while c != NONE {
            v.push(c);
            c = self.parent[c as usize];
        }
        v.reverse();
        v
    }
    /// Lowercased full path, for path searches.
    fn lpath(&self, i: u32) -> Vec<u8> {
        let a = self.ancestors(i);
        let mut p = Vec::with_capacity(a.len() * 16);
        for &c in &a[1..] {
            p.push(b'/');
            p.extend_from_slice(self.lname(c));
        }
        if p.is_empty() {
            p.push(b'/');
        }
        p
    }
    fn path(&self, i: u32) -> String {
        let a = self.ancestors(i);
        if a.len() <= 1 {
            return "/".into();
        }
        let mut s = String::new();
        for &c in &a[1..] {
            s.push('/');
            s.push_str(self.name(c));
        }
        s
    }
    fn find_path(&self, p: &str) -> Option<u32> {
        let mut cur = 0u32;
        for comp in p.split('/').filter(|s| !s.is_empty()) {
            cur = self.kids(cur).find(|&c| self.name(c) == comp)?;
        }
        Some(cur)
    }
    fn col(&self, v: &View, i: u32) -> [u8; 3] {
        match self.kind[i as usize] {
            FILE => v.ext_col[self.ext[i as usize] as usize],
            DIR => [90, 90, 90],
            _ => OTHER_COL,
        }
    }
    fn compute_ext_stats(&self) -> Vec<Vec<(u64, u32)>> {
        let mut v = vec![vec![(0u64, 0u32); self.exts.len()]; self.drives.len()];
        for i in 0..self.len() {
            if self.kind[i] == FILE && !self.dead[i] {
                let s = &mut v[self.drive[i] as usize][self.ext[i] as usize];
                s.0 += self.alloc[i];
                s.1 += 1;
            }
        }
        v
    }
    /// Per-extension totals over the drives marked in `show`.
    fn ext_total(&self, show: &[bool]) -> Vec<(u64, u32)> {
        let mut v = vec![(0u64, 0u32); self.exts.len()];
        for (d, st) in self.ext_stats.iter().enumerate() {
            if show.get(d).copied().unwrap_or(true) {
                for (t, s) in v.iter_mut().zip(st) {
                    t.0 += s.0;
                    t.1 += s.1;
                }
            }
        }
        v
    }

    /// Palette colours for the extensions taking the most space on the shown drives; the rest are grey.
    fn ext_colors(&self, show: &[bool]) -> Vec<[u8; 3]> {
        let total = self.ext_total(show);
        let mut order: Vec<usize> = (0..self.exts.len()).filter(|&e| total[e].0 > 0).collect();
        let by_size = |a: &usize, b: &usize| total[*b].0.cmp(&total[*a].0);
        if order.len() > PALETTE.len() {
            order.select_nth_unstable_by(PALETTE.len(), by_size);
            order.truncate(PALETTE.len());
        }
        order.sort_unstable_by(by_size);
        let mut col = vec![OTHER_COL; self.exts.len()];
        for (rank, &e) in order.iter().enumerate() {
            col[e] = PALETTE[rank];
        }
        col
    }

    /// Tags every node with the drive it lives on; `mps` are the composed drives.
    fn set_drives(&mut self, mps: &[String]) {
        let mut found: Vec<(String, u32)> = mps.iter().filter(|mp| *mp != "/").filter_map(|mp| Some((mp.clone(), self.find_path(mp)?))).collect();
        found.truncate(u8::MAX as usize - 1);
        self.drives = std::iter::once("/".to_string()).chain(found.iter().map(|f| f.0.clone())).collect();
        self.dmount = std::iter::once(0).chain(found.iter().map(|f| f.1)).collect();
        let at: HashMap<u32, u8> = found.iter().enumerate().map(|(k, f)| (f.1, k as u8 + 1)).collect();
        // Parents come before their children (the build is breadth-first).
        for i in 1..self.len() {
            self.drive[i] = at.get(&(i as u32)).copied().unwrap_or(self.drive[self.parent[i] as usize]);
        }
        self.ext_stats = self.compute_ext_stats();
    }

    /// Fills `partial`. `scanned` maps each composed drive to its chosen folders (none for all of it).
    fn set_partial(&mut self, m: &drives::Mounts, scanned: &HashMap<&str, &[String]>) {
        // Whether the drive a folder physically lives on was scanned there.
        let covered = |p: &str| {
            let p = Path::new(p);
            m.holder(p).and_then(|d| scanned.get(d.mp.as_str())).is_some_and(|f| f.is_empty() || f.iter().any(|f| p.starts_with(f)))
        };
        let chosen = scanned.values().flat_map(|f| f.iter()).filter_map(|f| self.find_path(f));
        let tops: Vec<u32> = self.dmount[1..].iter().copied().chain(chosen).collect();
        let (mut part, mut seen) = (HashSet::new(), HashSet::new());
        for top in tops {
            let mut c = self.parent[top as usize];
            while c != NONE && seen.insert(c) {
                if !covered(&self.path(c)) {
                    part.insert(c);
                }
                c = self.parent[c as usize];
            }
        }
        self.partial = part;
    }

    /// A view showing only the drives not named in `hide`.
    fn view(&self, hide: &[&str]) -> View {
        let show: Vec<bool> = self.drives.iter().map(|d| !hide.contains(&d.as_str())).collect();
        let mut eff = HashMap::new();
        let ext_col = self.ext_colors(&show);
        if show.iter().all(|&s| s) {
            return View { show, eff, ext_col };
        }
        // Only mount points and the folders above them mix drives; everything else is all or nothing.
        let mut mixed: Vec<u32> = self.dmount.iter().flat_map(|&m| self.ancestors(m)).collect();
        mixed.sort_unstable();
        mixed.dedup();
        let (mut raw, mut sub) = (HashMap::<u32, Agg>::new(), HashMap::<u32, Agg>::new());
        // Children have higher ids than their parents, so this sees children first.
        for &i in mixed.iter().rev() {
            let u = i as usize;
            if self.dead[u] {
                continue;
            }
            let r = raw.remove(&i).unwrap_or_default();
            let mut a = sub.remove(&i).unwrap_or_default();
            if show[self.drive[u] as usize] {
                a.size += self.size[u].saturating_sub(r.size);
                a.alloc += self.alloc[u].saturating_sub(r.alloc);
                a.files += self.files[u].saturating_sub(r.files);
                a.vis = true;
            }
            eff.insert(i, a);
            let p = self.parent[u];
            if p != NONE {
                raw.entry(p).or_default().add(&Agg { size: self.size[u], alloc: self.alloc[u], files: self.files[u], vis: false });
                sub.entry(p).or_default().add(&a);
            }
        }
        View { show, eff, ext_col }
    }
    fn alloc_in(&self, v: &View, i: u32) -> u64 {
        v.eff.get(&i).map_or(self.alloc[i as usize], |a| a.alloc)
    }
    /// Live children visible in `v`, largest first.
    fn kids_in(&self, v: &View, i: u32) -> Vec<u32> {
        let mut k: Vec<u32> = self.kids(i).filter(|&c| v.vis(self, c)).collect();
        if !v.eff.is_empty() {
            k.sort_by_key(|&c| std::cmp::Reverse(self.alloc_in(v, c)));
        }
        k
    }

    fn item(&self, v: &View, i: u32) -> Value {
        let u = i as usize;
        let c = self.col(v, i);
        let a = v.eff.get(&i).copied().unwrap_or(Agg { size: self.size[u], alloc: self.alloc[u], files: self.files[u], vis: true });
        let mut r = json!({
            "id": i, "name": self.name(i), "kind": self.kind[u], "size": a.size,
            "alloc": a.alloc, "files": a.files, "mtime": self.mtime[u],
            "ext": self.ext[u], "nk": self.nkids[u] > 0,
            "col": format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]),
        });
        if self.partial.contains(&i) {
            r["part"] = json!(true);
        }
        r
    }

    /// Remove a node (and its subtree) from the index after it was deleted on disk.
    fn kill(&mut self, id: u32) {
        let (s, a, f) = (self.size[id as usize], self.alloc[id as usize], self.files[id as usize]);
        let mut stack = vec![id];
        while let Some(c) = stack.pop() {
            let cu = c as usize;
            self.dead[cu] = true;
            if self.kind[cu] == DIR {
                self.dirs = self.dirs.saturating_sub(1);
            }
            let fst = self.first[cu];
            stack.extend(fst..fst + self.nkids[cu]);
        }
        let mut p = self.parent[id as usize];
        while p != NONE {
            let pu = p as usize;
            self.size[pu] = self.size[pu].saturating_sub(s);
            self.alloc[pu] = self.alloc[pu].saturating_sub(a);
            self.files[pu] = self.files[pu].saturating_sub(f);
            p = self.parent[pu];
        }
        self.ext_stats = self.compute_ext_stats();
    }

    fn search(&self, v: &View, q: &str, sort: &str, desc: bool, limit: usize) -> (usize, Vec<u32>) {
        let mut terms: Vec<String> = vec![];
        let mut pterms: Vec<String> = vec![];
        let mut exts: Option<Vec<u16>> = None;
        let (mut min, mut max, mut kind) = (None::<u64>, None::<u64>, None::<u8>);
        for tok in q.split_whitespace() {
            let tl = tok.to_lowercase();
            if let Some(r) = tl.strip_prefix("ext:") {
                let v = exts.get_or_insert_with(Vec::new);
                v.extend(r.split([',', ';']).filter_map(|e| self.ext_ids.get(e.trim_start_matches('.')).copied()));
            } else if let Some(r) = tl.strip_prefix("size:") {
                if let Some(n) = r.strip_prefix('>').and_then(parse_size) {
                    min = Some(n);
                } else if let Some(n) = r.strip_prefix('<').and_then(parse_size) {
                    max = Some(n);
                }
            } else if tl == "dir:" || tl == "folder:" {
                kind = Some(DIR);
            } else if tl == "file:" {
                kind = Some(FILE);
            } else if tl.contains('/') {
                pterms.push(tl);
            } else {
                terms.push(tl);
            }
        }
        if terms.is_empty() && pterms.is_empty() && exts.is_none() && min.is_none() && max.is_none() && kind.is_none() {
            return (0, vec![]);
        }
        let fin: Vec<memmem::Finder> = terms.iter().map(|t| memmem::Finder::new(t.as_bytes())).collect();
        let pfin: Vec<memmem::Finder> = pterms.iter().map(|t| memmem::Finder::new(t.as_bytes())).collect();
        let mut ids: Vec<u32> = (1..self.len() as u32)
            .into_par_iter()
            .filter(|&i| {
                let u = i as usize;
                if self.dead[u] || !v.show[self.drive[u] as usize] || kind.is_some_and(|k| self.kind[u] != k) {
                    return false;
                }
                if let Some(es) = &exts
                    && (self.kind[u] != FILE || !es.contains(&self.ext[u]))
                {
                    return false;
                }
                let sz = self.size[u];
                if min.is_some_and(|m| sz <= m) || max.is_some_and(|m| sz >= m) {
                    return false;
                }
                let ln = self.lname(i);
                if !fin.iter().all(|f| f.find(ln).is_some()) {
                    return false;
                }
                if !pfin.is_empty() {
                    let p = self.lpath(i);
                    return pfin.iter().all(|f| f.find(&p).is_some());
                }
                true
            })
            .collect();
        let total = ids.len();
        match sort {
            "size" => ids.par_sort_unstable_by_key(|&i| self.size[i as usize]),
            "mtime" => ids.par_sort_unstable_by_key(|&i| self.mtime[i as usize]),
            _ => ids.par_sort_unstable_by(|&a, &b| self.lname(a).cmp(self.lname(b))),
        }
        if desc {
            ids.reverse();
        }
        ids.truncate(limit);
        (total, ids)
    }
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mul) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1u64 << 10),
        'm' => (&s[..s.len() - 1], 1 << 20),
        'g' => (&s[..s.len() - 1], 1 << 30),
        't' => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.parse::<f64>().ok().map(|n| (n * mul as f64) as u64)
}

// ---------------------------------------------------------------- treemap

const H0: f64 = 0.38;
const SCALE: f64 = 0.91;
const IA: f64 = 0.13;
const IS: f64 = 1.0 - IA;
const PALETTE_BRIGHTNESS: f64 = 0.6;
const BRIGHT: f64 = 0.88 / PALETTE_BRIGHTNESS;
// How much of the ancestors' cushion surface each level inherits. WinDirStat uses 1.0, which on
// deep Linux paths stacks ~10 near-identical ridges into near-black edges.
const DECAY: f64 = 0.75;

/// Push channel overflow above 255 into the other channels, so bright colours wash towards white
/// instead of clipping (WinDirStat's DistributeFirst).
fn distribute(v: &mut [f64; 3]) {
    for _ in 0..2 {
        for i in 0..3 {
            if v[i] > 255.0 {
                let ex = v[i] - 255.0;
                v[i] = 255.0;
                v[(i + 1) % 3] += ex / 2.0;
                v[(i + 2) % 3] += ex / 2.0;
            }
        }
    }
    for c in v.iter_mut() {
        *c = c.min(255.0);
    }
}

/// Scale a palette colour to a fixed average brightness so every extension reads equally light.
fn normalize(c: [u8; 3]) -> [f64; 3] {
    let sum = (c[0] as f64 + c[1] as f64 + c[2] as f64).max(1.0);
    let f = PALETTE_BRIGHTNESS * 3.0 * 255.0 / sum;
    let mut v = [c[0] as f64 * f, c[1] as f64 * f, c[2] as f64 * f];
    distribute(&mut v);
    v
}

fn ridge(r: [f64; 4], mut s: [f64; 4], h: f64) -> [f64; 4] {
    let (w, hh) = (r[2] - r[0], r[3] - r[1]);
    if w > 0.0 {
        let wf = 4.0 * h / w;
        s[2] -= wf;
        s[0] += wf * (r[2] + r[0]);
    }
    if hh > 0.0 {
        let hf = 4.0 * h / hh;
        s[3] -= hf;
        s[1] += hf * (r[3] + r[1]);
    }
    s
}

/// Squarified layout of `sizes` (sorted descending) into rect `r` = [x0,y0,x1,y1].
fn squarify(sizes: &[u64], r: [f64; 4], out: &mut Vec<[f64; 4]>) {
    let [mut x0, mut y0, x1, y1] = r;
    let mut remaining: u64 = sizes.iter().sum();
    let n = sizes.len();
    let mut i = 0;
    while i < n {
        let (w, h) = (x1 - x0, y1 - y0);
        if remaining == 0 || w <= 0.0 || h <= 0.0 {
            out.extend((i..n).map(|_| [x0, y0, x0, y0]));
            return;
        }
        let scale = w * h / remaining as f64;
        let horiz = w >= h; // lay a column along the left edge
        let side = if horiz { h } else { w };
        let amax = sizes[i] as f64 * scale;
        let (mut j, mut sum, mut sumb, mut best) = (i, 0.0, 0u64, f64::INFINITY);
        while j < n {
            let a = sizes[j] as f64 * scale;
            let ns = sum + a;
            let worst = f64::max(side * side * amax / (ns * ns), ns * ns / (side * side * a));
            if j > i && worst > best {
                break;
            }
            best = worst;
            sum = ns;
            sumb += sizes[j];
            j += 1;
        }
        let last = j == n;
        let thick = if last { if horiz { w } else { h } } else { sum / side };
        let mut pos = if horiz { y0 } else { x0 };
        for (k, &sz) in sizes.iter().enumerate().take(j).skip(i) {
            let mut end = pos + sz as f64 * scale / thick;
            if k == j - 1 {
                end = if horiz { y1 } else { x1 };
            }
            out.push(if horiz { [x0, pos, x0 + thick, end] } else { [pos, y0, end, y0 + thick] });
            pos = end;
        }
        if horiz { x0 += thick } else { y0 += thick }
        remaining -= sumb;
        i = j;
    }
}

#[derive(Clone, Copy, Default)]
struct Agg {
    size: u64,
    alloc: u64,
    files: u32,
    vis: bool,
}
impl Agg {
    fn add(&mut self, o: &Agg) {
        self.size += o.size;
        self.alloc += o.alloc;
        self.files += o.files;
        self.vis |= o.vis;
    }
}

/// The drives the UI shows. `eff` holds recomputed totals for folders whose subtrees mix
/// shown and hidden drives; it is empty when every drive is shown.
struct View {
    show: Vec<bool>,
    eff: HashMap<u32, Agg>,
    /// Colour per extension, ranked by space on the shown drives.
    ext_col: Vec<[u8; 3]>,
}
impl View {
    fn vis(&self, ix: &Index, i: u32) -> bool {
        self.eff.get(&i).map_or_else(|| self.show[ix.drive[i as usize] as usize], |a| a.vis)
    }
}

struct Tm<'a> {
    ix: &'a Index,
    v: &'a View,
    w: usize,
    h: usize,
    px: Vec<u8>,
    rects: Vec<u8>,
    n: u32,
    light: [f64; 3],
}

impl Tm<'_> {
    fn rec(&mut self, id: u32, r: [f64; 4], surf: [f64; 4], hgt: f64, depth: u8) {
        let x0 = (r[0].round().max(0.0) as usize).min(self.w);
        let y0 = (r[1].round().max(0.0) as usize).min(self.h);
        let x1 = (r[2].round().max(0.0) as usize).min(self.w);
        let y1 = (r[3].round().max(0.0) as usize).min(self.h);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let s = ridge(r, surf.map(|v| v * DECAY), hgt);
        let ix = self.ix;
        let u = id as usize;
        for v in [x0 as u16, y0 as u16, x1 as u16, y1 as u16] {
            self.rects.extend_from_slice(&v.to_le_bytes());
        }
        self.rects.extend_from_slice(&id.to_le_bytes());
        self.rects.extend_from_slice(&ix.ext[u].to_le_bytes());
        self.rects.push(depth);
        self.rects.push(ix.kind[u]);
        self.n += 1;
        let kids: Vec<u32> = if ix.kind[u] == DIR { ix.kids_in(self.v, id).into_iter().filter(|&c| ix.alloc_in(self.v, c) > 0).collect() } else { vec![] };
        if kids.is_empty() {
            self.paint(x0, y0, x1, y1, &s, ix.col(self.v, id));
            return;
        }
        let sizes: Vec<u64> = kids.iter().map(|&c| ix.alloc_in(self.v, c)).collect();
        let mut out = Vec::with_capacity(kids.len());
        squarify(&sizes, r, &mut out);
        for (&k, cr) in kids.iter().zip(out) {
            self.rec(k, cr, s, hgt * SCALE, depth.saturating_add(1));
        }
    }

    fn paint(&mut self, x0: usize, y0: usize, x1: usize, y1: usize, s: &[f64; 4], col: [u8; 3]) {
        let [lx, ly, lz] = self.light;
        let n = normalize(col);
        let cf = [n[0] * BRIGHT, n[1] * BRIGHT, n[2] * BRIGHT];
        for y in y0..y1 {
            let ny = -(2.0 * s[3] * (y as f64 + 0.5) + s[1]);
            let row = y * self.w;
            for x in x0..x1 {
                let nx = -(2.0 * s[2] * (x as f64 + 0.5) + s[0]);
                let cosa = ((nx * lx + ny * ly + lz) / (nx * nx + ny * ny + 1.0).sqrt()).min(1.0);
                let p = (IS * cosa).max(0.0) + IA;
                let o = (row + x) * 4;
                let mut v = [cf[0] * p, cf[1] * p, cf[2] * p];
                distribute(&mut v);
                self.px[o] = v[0] as u8;
                self.px[o + 1] = v[1] as u8;
                self.px[o + 2] = v[2] as u8;
            }
        }
    }
}

/// Binary: [w,h,nrects,epoch] u32 LE, RGBA pixels, then 16-byte rects
/// (x0,y0,x1,y1 u16; id u32; ext u16; depth u8; kind u8) in pre-order.
fn treemap(ix: &Index, v: &View, root: u32, w: usize, h: usize) -> Vec<u8> {
    let l = (1.0f64 + 1.0 + 100.0).sqrt();
    let mut px = vec![0u8; w * h * 4];
    for p in px.as_chunks_mut::<4>().0 {
        *p = [12, 13, 16, 255];
    }
    let mut tm = Tm { ix, v, w, h, px, rects: Vec::new(), n: 0, light: [-1.0 / l, -1.0 / l, 10.0 / l] };
    if ix.alloc_in(v, root) > 0 || ix.kind[root as usize] != DIR {
        tm.rec(root, [0.0, 0.0, w as f64, h as f64], [0.0; 4], H0, 0);
    }
    let mut out = Vec::with_capacity(16 + tm.px.len() + tm.rects.len());
    for v in [w as u32, h as u32, tm.n, ix.epoch as u32] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&tm.px);
    out.extend_from_slice(&tm.rects);
    out
}

// ---------------------------------------------------------------- daemon

struct State {
    idx: RwLock<Index>,
    epoch: AtomicU64,
    cfg: Mutex<HashMap<String, DriveCfg>>,
    /// Last scan of each drive, from its cache header or a finished scan.
    meta: Mutex<HashMap<String, Meta>>,
    queue: Mutex<VecDeque<String>>,
    wake: Condvar,
    current: Mutex<Option<String>>,
    /// Drives whose trees make up the live index.
    composed: Mutex<Vec<String>>,
    rebuilding: Mutex<()>,
    /// Drives whose last scan could not read them (an unreachable share, say): when, and why.
    failed: Mutex<HashMap<String, (u64, String)>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// Lock order: `queue` before `current`.
impl State {
    fn cfg_of(&self, mp: &str) -> DriveCfg {
        lock(&self.cfg).get(mp).cloned().unwrap_or_else(|| DriveCfg::default_for(mp))
    }
    /// Mounted drives plus the folders added by hand.
    fn mounts(&self) -> drives::Mounts {
        let mut m = drives::mounts();
        m.add_folders(lock(&self.cfg).iter().filter(|(_, c)| c.added).map(|(mp, _)| mp));
        m
    }
    fn busy(&self, mp: &str) -> bool {
        let q = lock(&self.queue);
        q.iter().any(|m| m == mp) || lock(&self.current).as_deref() == Some(mp)
    }
    fn scanning(&self) -> bool {
        let q = lock(&self.queue);
        !q.is_empty() || lock(&self.current).is_some()
    }
    /// Enabled drives that are mounted right now, "/" first.
    fn active(&self, m: &drives::Mounts) -> Vec<String> {
        m.drives.iter().filter(|d| self.cfg_of(&d.mp).enabled).map(|d| d.mp.clone()).collect()
    }
}

fn queue_scan(st: &State, mp: &str) {
    let mut q = lock(&st.queue);
    if !q.iter().any(|m| m == mp) && lock(&st.current).as_deref() != Some(mp) {
        q.push_back(mp.to_string());
        st.wake.notify_one();
    }
}

fn queue_all(st: &State) {
    for mp in st.active(&st.mounts()) {
        queue_scan(st, &mp);
    }
}

/// A directory holding `children`, with their totals.
fn folder(name: &str, mtime: i64, children: Vec<Tmp>) -> Tmp {
    let mut t = Tmp { name: name.into(), kind: DIR, size: 0, alloc: 0, mtime, files: 0, children };
    for c in &t.children {
        t.size += c.size;
        t.alloc += c.alloc;
        t.files += c.files;
    }
    t
}

/// Scans the drive at `mp`, or only the folders in `paths` when there are any.
fn scan_drive(mp: &str, paths: &[String], m: &drives::Mounts) -> (Tmp, Meta) {
    let t = Instant::now();
    let mut skip = m.pseudo.clone();
    skip.extend(m.drives.iter().filter(|d| d.mp != mp).map(|d| PathBuf::from(&d.mp)));
    let reflink = m.drives.iter().find(|d| d.mp == mp).is_some_and(|d| extents::REFLINK_FS.contains(&d.fstype.as_str()));
    let ctx = Ctx { skip, seen: Mutex::new(HashSet::new()), reflink: reflink.then(Default::default), errors: AtomicU64::new(0) };
    let mtime = |p: &str| fs::metadata(p).map(|m| m.mtime()).unwrap_or(0);
    let name = Path::new(mp).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
    let root = if paths.is_empty() {
        folder(&name, mtime(mp), scan_dir(Path::new(mp), &ctx))
    } else {
        // Each chosen folder hangs under just the directories that lead to it.
        let mut root = folder(&name, mtime(mp), Vec::new());
        for p in paths {
            let Some(rel) = Path::new(p).strip_prefix(mp).ok().and_then(Path::to_str) else { continue };
            if !Path::new(p).is_dir() {
                ctx.errors.fetch_add(1, SeqCst);
                continue;
            }
            let comps: Vec<&str> = rel.split('/').filter(|s| !s.is_empty()).collect();
            drives::graft(&mut root, Path::new(mp), &comps, folder("", mtime(p), scan_dir(Path::new(p), &ctx)));
        }
        root
    };
    (root, Meta { paths: paths.to_vec(), ..drives::fresh_meta(t.elapsed().as_millis() as u64, ctx.errors.load(SeqCst)) })
}

/// Recomposes the live index from every active drive's cache (or `fresh`, a tree just scanned).
fn rebuild(st: &State, m: &drives::Mounts, mut fresh: Option<(String, Tmp)>) {
    let _g = lock(&st.rebuilding);
    let t = Instant::now();
    let mut parts = Vec::new();
    for mp in st.active(m) {
        if fresh.as_ref().is_some_and(|f| f.0 == mp) {
            parts.extend(fresh.take());
        } else if let Some((tree, _)) = drives::load_cache(&mp) {
            parts.push((mp, tree));
        } else if lock(&st.meta).remove(&mp).is_some() {
            eprintln!("everdir: cache for {mp} unreadable; it will be rescanned");
        }
    }
    let included: Vec<String> = parts.iter().map(|p| p.0.clone()).collect();
    let metas: Vec<(&str, Meta)> = {
        let meta = lock(&st.meta);
        included.iter().filter_map(|mp| Some((mp.as_str(), meta.get(mp)?.clone()))).collect()
    };
    let root = drives::compose(parts);
    let epoch = st.epoch.fetch_add(1, SeqCst) + 1;
    let mut ix = Index::build(
        root,
        epoch,
        metas.iter().map(|m| m.1.scanned_at).min().unwrap_or(0),
        metas.iter().map(|m| m.1.scan_ms).sum(),
        metas.iter().map(|m| m.1.errors).sum(),
    );
    ix.set_drives(&included);
    ix.set_partial(m, &metas.iter().map(|(mp, x)| (*mp, x.paths.as_slice())).collect());
    eprintln!("index #{epoch}: {} nodes from {included:?} in {} ms", ix.len(), t.elapsed().as_millis());
    let old = std::mem::replace(&mut *wr(&st.idx), ix);
    drop(old);
    *lock(&st.composed) = included;
    release_memory();
}

/// Hands freed heap pages back to the OS. The scan tree and the previous index are millions of
/// small allocations spread over rayon's per-thread arenas, which glibc otherwise keeps resident.
fn release_memory() {
    #[cfg(target_env = "gnu")]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> i32;
        }
        // SAFETY: malloc_trim only releases free memory; it has no preconditions.
        unsafe { malloc_trim(0) };
    }
}

/// Whether a drive's root can be listed, noting why not in `failed`. An unreachable share, or
/// an added folder that is gone, keeps its last scan instead of an empty one.
fn readable(st: &State, mp: &str) -> bool {
    // On a share, opening a folder can succeed when listing it then fails.
    match fs::read_dir(mp).and_then(|mut r| r.next().transpose()) {
        Ok(_) => {
            lock(&st.failed).remove(mp);
            true
        }
        Err(e) => {
            eprintln!("everdir: cannot read {mp}: {e}; keeping its last scan");
            lock(&st.failed).insert(mp.to_string(), (now(), e.to_string()));
            false
        }
    }
}

fn worker(st: Arc<State>) {
    loop {
        let mp = {
            let mut q = lock(&st.queue);
            loop {
                if let Some(mp) = q.pop_front() {
                    *lock(&st.current) = Some(mp.clone());
                    break mp;
                }
                q = st.wake.wait(q).unwrap_or_else(|e| e.into_inner());
            }
        };
        let m = st.mounts();
        let cfg = st.cfg_of(&mp);
        if m.drives.iter().any(|d| d.mp == mp) && cfg.enabled && readable(&st, &mp) {
            let (tree, mut meta) = scan_drive(&mp, &cfg.paths, &m);
            eprintln!("scan {mp}: {} files in {} ms, {} unreadable dirs", tree.files, meta.scan_ms, meta.errors);
            // Disabled while it was being scanned: throw the result away.
            if st.cfg_of(&mp).enabled {
                if let Err(e) = drives::save_cache(&mp, &tree, &mut meta) {
                    eprintln!("everdir: cannot write cache for {mp}: {e}");
                }
                lock(&st.meta).insert(mp.clone(), meta);
                rebuild(&st, &m, Some((mp.clone(), tree)));
            }
        }
        *lock(&st.current) = None;
        // Its folders were changed during the scan: scan again with the new ones.
        let now_cfg = st.cfg_of(&mp);
        if now_cfg.enabled && now_cfg.paths != cfg.paths {
            queue_scan(&st, &mp);
        }
    }
}

/// Queues drives that are due and recomposes the index when the set of active drives changed.
fn tick(st: &State) {
    let m = st.mounts();
    let active = st.active(&m);
    let meta = lock(&st.meta).clone();
    let failed = lock(&st.failed).clone();
    for mp in &active {
        let cfg = st.cfg_of(mp);
        let due = meta.get(mp).is_none_or(|x| x.paths != cfg.paths || cfg.every > 0 && now() >= x.scanned_at + cfg.every);
        if due && failed.get(mp).is_none_or(|f| now() >= f.0 + RETRY_AFTER) {
            queue_scan(st, mp);
        }
    }
    let cached: Vec<String> = active.into_iter().filter(|mp| meta.contains_key(mp)).collect();
    if cached != *lock(&st.composed) && !cached.is_empty() {
        rebuild(st, &m, None);
    }
}

/// Writes a drive's cache back out after a delete, so the file stays gone after a restart.
fn persist_delete(st: &Arc<State>, path: &str) {
    let (st, path) = (st.clone(), path.to_string());
    std::thread::spawn(move || {
        let composed = lock(&st.composed).clone();
        let Some(mp) = composed.iter().filter(|mp| Path::new(&path).starts_with(mp)).max_by_key(|mp| mp.len()).cloned() else { return };
        if st.busy(&mp) {
            return; // a rescan is coming that will see the delete anyway
        }
        let _g = lock(&st.rebuilding);
        let tree = {
            let ix = rd(&st.idx);
            let Some(id) = ix.find_path(&mp) else { return };
            let stop: HashSet<u32> = composed.iter().filter(|o| **o != mp).filter_map(|o| ix.find_path(o)).collect();
            drives::export(&ix, id, &stop)
        };
        let mut meta = lock(&st.meta).get(&mp).cloned().unwrap_or_default();
        match drives::save_cache(&mp, &tree, &mut meta) {
            Ok(()) => {
                lock(&st.meta).insert(mp, meta);
            }
            Err(e) => eprintln!("everdir: cannot write cache for {mp}: {e}"),
        }
    });
}

fn drives_json(st: &State) -> Value {
    let m = st.mounts();
    let cfg = lock(&st.cfg).clone();
    let meta = lock(&st.meta).clone();
    let queued: Vec<String> = lock(&st.queue).iter().cloned().collect();
    let current = lock(&st.current).clone();
    let failed = lock(&st.failed).clone();
    let scan = |mp: &str| {
        meta.get(mp).map(|x| {
            json!({ "scanned_at": x.scanned_at, "scan_ms": x.scan_ms, "errors": x.errors, "nodes": x.nodes, "alloc": x.alloc, "files": x.files })
        })
    };
    let row = |mp: &str, c: &DriveCfg| {
        json!({
            "mp": mp, "root": mp == "/", "enabled": c.enabled, "every": c.every, "paths": c.paths, "scan": scan(mp),
            "scanning": current.as_deref() == Some(mp), "queued": queued.iter().any(|q| q == mp),
            "failed": failed.get(mp).map(|f| &f.1), "added": c.added,
        })
    };
    let mut out: Vec<Value> = m
        .drives
        .iter()
        .map(|d| {
            let mut v = row(&d.mp, &st.cfg_of(&d.mp));
            v["mounted"] = json!(true);
            v["source"] = json!(d.source);
            v["fstype"] = json!(d.fstype);
            v["disk"] = json!(d.disk);
            v["model"] = json!(d.model);
            v["hdd"] = json!(d.rotational);
            v["dev_size"] = json!(d.dev_size);
            v["network"] = json!(d.remote);
            if let Some((size, used)) = drives::usage_of(d) {
                (v["disk_size"], v["disk_used"]) = (json!(size), json!(used));
            }
            v
        })
        .collect();
    let mut gone: Vec<(&String, &DriveCfg)> = cfg.iter().filter(|(mp, _)| !m.drives.iter().any(|d| &d.mp == *mp)).collect();
    gone.sort_by_key(|g| g.0);
    for (mp, c) in gone {
        let mut v = row(mp, c);
        v["mounted"] = json!(false);
        out.push(v);
    }
    json!({ "drives": out })
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' => {
                match std::str::from_utf8(b.get(i + 1..i + 3).unwrap_or(b"")).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(v) => {
                        out.push(v);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pct_encode(s: &str, keep_slash: bool) -> String {
    let mut o = String::new();
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || b"-_.~".contains(&c) || (keep_slash && c == b'/') {
            o.push(c as char);
        } else {
            o.push_str(&format!("%{c:02X}"));
        }
    }
    o
}

fn spawn_detached(cmd: &str, args: &[&str]) -> bool {
    match Command::new(cmd).args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn() {
        Ok(mut ch) => {
            std::thread::spawn(move || ch.wait());
            true
        }
        Err(_) => false,
    }
}

struct Reply(u16, &'static str, Vec<u8>);

fn ok(v: Value) -> Reply {
    Reply(200, "application/json", v.to_string().into_bytes())
}
fn err(code: u16, msg: &str) -> Reply {
    Reply(code, "application/json", json!({ "error": msg }).to_string().into_bytes())
}

fn handle(st: &Arc<State>, mut req: Request) {
    let host_ok = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("Host"))
        .is_some_and(|h| {
            let v = h.value.as_str();
            ["127.0.0.1", "localhost", "everdir.localhost"].iter().any(|h| v == format!("{h}:{}", port()))
        });
    let is_post = *req.method() == Method::Post;
    let csrf_ok = !is_post || req.headers().iter().any(|h| h.field.equiv("X-Everdir"));
    let url = req.url().to_string();
    let (path, qs) = url.split_once('?').unwrap_or((&url, ""));
    let q: HashMap<String, String> = qs
        .split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (pct_decode(k), pct_decode(v))
        })
        .collect();
    let mut body = String::new();
    let _ = req.as_reader().take(1 << 16).read_to_string(&mut body);
    let r = if !host_ok || !csrf_ok { err(403, "forbidden") } else { route(st, is_post, path, &q) };
    let resp = Response::from_data(r.2)
        .with_status_code(r.0)
        .with_header(Header::from_bytes("Content-Type", r.1).unwrap())
        .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
    let _ = req.respond(resp);
}

fn route(st: &Arc<State>, post: bool, path: &str, q: &HashMap<String, String>) -> Reply {
    let num = |k: &str| q.get(k).and_then(|v| v.parse::<u64>().ok());
    if !post && path == "/" {
        return Reply(200, "text/html; charset=utf-8", HTML.as_bytes().to_vec());
    }
    if !post && path == "/favicon.svg" {
        return Reply(200, "image/svg+xml", ICON.as_bytes().to_vec());
    }
    // Drives the UI hides, newline-separated mount points.
    let hide: Vec<&str> = q.get("hide").map(|h| h.split('\n').filter(|s| !s.is_empty()).collect()).unwrap_or_default();
    if path == "/api/status" {
        let m = st.mounts();
        let ix = rd(&st.idx);
        let v = ix.view(&hide);
        let root = (ix.len() > 0).then(|| ix.item(&v, 0));
        let all: Vec<&str> = ix.drives.iter().map(String::as_str).collect();
        // Each drive's own share: everything with the other drives hidden.
        let drives: Vec<Value> = ix
            .drives
            .iter()
            .enumerate()
            .map(|(k, mp)| {
                let others: Vec<&str> = all.iter().enumerate().filter(|&(j, _)| j != k).map(|(_, m)| *m).collect();
                let a = ix.item(&ix.view(&others), 0);
                let (disk_size, disk_used) = m.drives.iter().find(|d| d.mp == *mp).and_then(drives::usage_of).unzip();
                json!({ "mp": mp, "alloc": a["alloc"], "files": a["files"], "shown": v.show[k], "disk_size": disk_size, "disk_used": disk_used })
            })
            .collect();
        return ok(json!({
            "gen": ix.epoch, "scanning": st.scanning(), "scanning_mp": lock(&st.current).clone(),
            "files": root.as_ref().map_or(json!(0), |r| r["files"].clone()), "alloc": root.as_ref().map_or(json!(0), |r| r["alloc"].clone()),
            "dirs": ix.dirs, "scanned_at": ix.scanned_at, "scan_ms": ix.scan_ms, "errors": ix.errors, "drives": drives,
        }));
    }
    if post && path == "/api/quit" {
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(100)); // let the reply go out first
            std::process::exit(0);
        });
        return ok(json!({ "ok": true }));
    }
    if post && path == "/api/rescan" {
        match q.get("mp") {
            Some(mp) if st.active(&st.mounts()).contains(mp) => queue_scan(st, mp),
            Some(_) => return err(400, "drive is not enabled or not mounted"),
            // Without a drive, the ones the UI shows.
            None => st.active(&st.mounts()).iter().filter(|mp| !hide.contains(&mp.as_str())).for_each(|mp| queue_scan(st, mp)),
        }
        return ok(json!({ "ok": true }));
    }
    if !post && path == "/api/drives" {
        return ok(drives_json(st));
    }
    // A folder of any kind (one on a network share, say) indexed as a drive of its own.
    if post && path == "/api/drives/add" {
        let p = match drives::check_folder(q.get("path").map(String::as_str).unwrap_or(""), &st.mounts()) {
            Ok(p) => p,
            Err(e) => return err(400, &e),
        };
        {
            let mut cfg = lock(&st.cfg);
            cfg.insert(p, DriveCfg { enabled: true, every: drives::DEFAULT_OTHER_EVERY, paths: Vec::new(), added: true });
            if let Err(e) = drives::save_config(&cfg) {
                return err(500, &format!("cannot save settings: {e}"));
            }
        }
        let bg = st.clone();
        std::thread::spawn(move || tick(&bg));
        return ok(drives_json(st));
    }
    if post && path == "/api/drives/remove" {
        let Some(mp) = q.get("mp").cloned() else { return err(400, "mp missing") };
        {
            let mut cfg = lock(&st.cfg);
            if !cfg.get(&mp).is_some_and(|c| c.added) {
                return err(400, "only added folders can be removed");
            }
            cfg.remove(&mp);
            if let Err(e) = drives::save_config(&cfg) {
                return err(500, &format!("cannot save settings: {e}"));
            }
        }
        drives::drop_cache(&mp);
        lock(&st.meta).remove(&mp);
        lock(&st.failed).remove(&mp);
        let bg = st.clone();
        std::thread::spawn(move || {
            let m = bg.mounts();
            rebuild(&bg, &m, None);
            // The drive holding the folder skipped it, so it is rescanned to bring it back.
            if let Some(h) = m.holder(Path::new(&mp)).filter(|h| bg.active(&m).contains(&h.mp)) {
                queue_scan(&bg, &h.mp);
            }
            tick(&bg);
        });
        return ok(drives_json(st));
    }
    if post && path == "/api/drives" {
        let Some(mp) = q.get("mp").cloned() else { return err(400, "mp missing") };
        // Folders to index instead of the whole drive, newline-separated.
        let paths = match q.get("paths").map(|p| drives::check_paths(&mp, p, &st.mounts())).transpose() {
            Ok(p) => p,
            Err(e) => return err(400, &e),
        };
        let was = st.cfg_of(&mp);
        let now_cfg = {
            let mut cfg = lock(&st.cfg);
            let c = cfg.entry(mp.clone()).or_insert_with(|| was.clone());
            if let Some(e) = q.get("enabled") {
                c.enabled = mp == "/" || e == "1";
            }
            if let Some(n) = num("every") {
                c.every = n;
            }
            if let Some(p) = paths {
                c.paths = p;
            }
            let c = c.clone();
            if let Err(e) = drives::save_config(&cfg) {
                return err(500, &format!("cannot save settings: {e}"));
            }
            c
        };
        if was.enabled && !now_cfg.enabled {
            drives::drop_cache(&mp);
            lock(&st.meta).remove(&mp);
        }
        let bg = st.clone();
        std::thread::spawn(move || {
            if was.enabled && !now_cfg.enabled {
                rebuild(&bg, &bg.mounts(), None);
            }
            tick(&bg);
        });
        return ok(drives_json(st));
    }
    // Everything below addresses nodes by id, which are only valid within one scan generation.
    let stale = |ix: &Index| num("gen").is_some_and(|g| g != ix.epoch);
    match (post, path) {
        (false, "/api/search") => {
            let ix = rd(&st.idx);
            let t = Instant::now();
            let limit = num("limit").unwrap_or(1000).min(20000) as usize;
            let v = ix.view(&hide);
            let (total, ids) = ix.search(
                &v,
                q.get("q").map(String::as_str).unwrap_or(""),
                q.get("sort").map(String::as_str).unwrap_or("name"),
                q.get("desc").is_some_and(|d| d == "1"),
                limit,
            );
            let results: Vec<Value> = ids
                .iter()
                .map(|&i| {
                    let mut r = ix.item(&v, i);
                    r["dir"] = json!(ix.path(ix.parent[i as usize]));
                    r
                })
                .collect();
            ok(json!({ "gen": ix.epoch, "total": total, "ms": t.elapsed().as_secs_f64() * 1000.0, "results": results }))
        }
        (false, "/api/children") => {
            let ix = rd(&st.idx);
            let id = num("id").unwrap_or(0) as u32;
            if stale(&ix) || !ix.valid(id) {
                return err(409, "stale");
            }
            let limit = match num("limit").unwrap_or(500) {
                0 => usize::MAX,
                n => n as usize,
            };
            let v = ix.view(&hide);
            let all = ix.kids_in(&v, id);
            let rest = &all[all.len().min(limit)..];
            ok(json!({
                "items": all.iter().take(limit).map(|&c| ix.item(&v, c)).collect::<Vec<_>>(),
                "more": rest.len(), "moreAlloc": rest.iter().map(|&c| ix.alloc_in(&v, c)).sum::<u64>(),
            }))
        }
        (false, "/api/node") => {
            let ix = rd(&st.idx);
            let id = match q.get("path") {
                Some(p) => match ix.find_path(p) {
                    Some(i) => i,
                    None => return err(404, "not found"),
                },
                None => num("id").unwrap_or(0) as u32,
            };
            if (q.get("path").is_none() && stale(&ix)) || !ix.valid(id) {
                return err(409, "stale");
            }
            let v = ix.view(&hide);
            if !v.vis(&ix, id) {
                return err(404, "on a hidden drive");
            }
            let mut r = ix.item(&v, id);
            r["path"] = json!(ix.path(id));
            if !q.contains_key("lite") {
                r["ancestors"] = json!(ix.ancestors(id).iter().map(|&a| ix.item(&v, a)).collect::<Vec<_>>());
            }
            ok(r)
        }
        (false, "/api/exts") => {
            let ix = rd(&st.idx);
            let v = ix.view(&hide);
            let stats = ix.ext_total(&v.show);
            let mut ids: Vec<usize> = (0..ix.exts.len()).filter(|&e| stats[e].1 > 0).collect();
            ids.sort_unstable_by(|&a, &b| stats[b].0.cmp(&stats[a].0));
            let total: u64 = stats.iter().map(|s| s.0).sum();
            let items: Vec<Value> = ids
                .iter()
                .take(300)
                .map(|&e| {
                    let c = v.ext_col[e];
                    json!({ "id": e, "ext": ix.exts[e], "alloc": stats[e].0, "count": stats[e].1,
                            "col": format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2]) })
                })
                .collect();
            ok(json!({ "total": total, "items": items }))
        }
        (false, "/api/treemap") => {
            let ix = rd(&st.idx);
            let id = num("id").unwrap_or(0) as u32;
            if stale(&ix) || !ix.valid(id) {
                return err(409, "stale");
            }
            let w = num("w").unwrap_or(800).clamp(1, 8192) as usize;
            let h = num("h").unwrap_or(600).clamp(1, 8192) as usize;
            let v = ix.view(&hide);
            let id = if v.vis(&ix, id) { id } else { 0 };
            Reply(200, "application/octet-stream", treemap(&ix, &v, id, w, h))
        }
        (true, "/api/delete") => {
            let id = num("id").unwrap_or(0) as u32;
            let p = {
                let ix = rd(&st.idx);
                if stale(&ix) || !ix.valid(id) {
                    return err(409, "stale");
                }
                if ix.ancestors(id).len() <= 2 {
                    return err(400, "refusing to delete / or a top-level directory");
                }
                // The index would understate what goes with it.
                if ix.partial.iter().any(|&p| ix.ancestors(p).contains(&id)) {
                    return err(400, "refusing to delete a folder that is only partly indexed");
                }
                ix.path(id)
            };
            let home = std::env::var("HOME").unwrap_or_default();
            if p == home || p.trim_end_matches('/') == home.trim_end_matches('/') {
                return err(400, "refusing to delete your home directory");
            }
            let res = if q.get("perm").is_some_and(|v| v == "1") {
                let md = fs::symlink_metadata(&p);
                match md {
                    Ok(m) if m.is_dir() => fs::remove_dir_all(&p).map_err(|e| e.to_string()),
                    Ok(_) => fs::remove_file(&p).map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                }
            } else {
                match trash_cmd(&p).output() {
                    Ok(o) if o.status.success() => Ok(()),
                    Ok(o) => Err(String::from_utf8_lossy(&o.stderr).trim().to_string()),
                    Err(e) => Err(e.to_string()),
                }
            };
            match res {
                Ok(()) => {
                    let mut ix = wr(&st.idx);
                    if ix.valid(id) && ix.path(id) == p {
                        ix.kill(id);
                    }
                    drop(ix);
                    persist_delete(st, &p);
                    ok(json!({ "ok": true }))
                }
                Err(e) => {
                    // A partial delete leaves the index inaccurate; a gone path means it's fully gone.
                    if fs::symlink_metadata(&p).is_err() {
                        let mut ix = wr(&st.idx);
                        if ix.valid(id) && ix.path(id) == p {
                            ix.kill(id);
                        }
                        drop(ix);
                        persist_delete(st, &p);
                    }
                    err(500, &e)
                }
            }
        }
        (true, "/api/open") => {
            let ix = rd(&st.idx);
            let id = num("id").unwrap_or(0) as u32;
            if stale(&ix) || !ix.valid(id) {
                return err(409, "stale");
            }
            let p = ix.path(id);
            let launched = if cfg!(target_os = "macos") {
                let reveal = q.get("how").is_some_and(|h| h == "reveal");
                spawn_detached("open", &(if reveal { vec!["-R", "--", &p] } else { vec!["--", &p] }))
            } else if q.get("how").is_some_and(|h| h == "reveal") {
                let uri = format!("array:string:file://{}", pct_encode(&p, true));
                let shown = Command::new("dbus-send")
                    .args(["--session", "--print-reply", "--dest=org.freedesktop.FileManager1", "--type=method_call",
                           "/org/freedesktop/FileManager1", "org.freedesktop.FileManager1.ShowItems", &uri, "string:"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                shown || spawn_detached("xdg-open", &[&ix.path(ix.parent[id as usize].min(id))])
            } else {
                spawn_detached("xdg-open", &[&p])
            };
            if launched { ok(json!({ "ok": true })) } else { err(500, "could not launch") }
        }
        _ => err(404, "not found"),
    }
}

/// Moves `p` to the Trash: through Finder on macOS (so "Put Back" works), gio elsewhere.
fn trash_cmd(p: &str) -> Command {
    if cfg!(target_os = "macos") {
        let mut c = Command::new("osascript");
        c.args(["-e", "on run argv", "-e", "tell application \"Finder\" to delete (POSIX file (item 1 of argv) as alias)", "-e", "end run", p]);
        c
    } else {
        let mut c = Command::new("gio");
        c.args(["trash", "--", p]);
        c
    }
}

fn serve() {
    let cfg = drives::load_config();
    let mut meta = HashMap::new();
    for mp in cfg.keys().cloned().chain(drives::mounts().drives.into_iter().map(|d| d.mp)) {
        if let Some(m) = drives::cache_meta(&mp) {
            meta.insert(mp, m);
        }
    }
    let st = Arc::new(State {
        idx: RwLock::new(Index::build(empty_root(), 0, 0, 0, 0)),
        epoch: AtomicU64::new(0),
        cfg: Mutex::new(cfg),
        meta: Mutex::new(meta),
        queue: Mutex::new(VecDeque::new()),
        wake: Condvar::new(),
        current: Mutex::new(None),
        composed: Mutex::new(Vec::new()),
        rebuilding: Mutex::new(()),
        failed: Mutex::new(HashMap::new()),
    });
    let server = Arc::new(match Server::http(("127.0.0.1", port())) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("everdir: cannot bind 127.0.0.1:{}: {e}", port());
            std::process::exit(1);
        }
    });
    eprintln!("everdir: serving http://127.0.0.1:{}", port());
    {
        let st = st.clone();
        std::thread::spawn(move || worker(st));
    }
    // The first tick loads the cached index; later ones start scans as drives fall due.
    {
        let st = st.clone();
        std::thread::spawn(move || {
            loop {
                tick(&st);
                std::thread::sleep(Duration::from_secs(60));
            }
        });
    }
    // Kept alive for the life of the daemon. `assume_sni_available` keeps the tray waiting for
    // plasmashell instead of failing when the service starts before the panel does.
    #[cfg(target_os = "linux")]
    let _tray = if std::env::var_os("EVERDIR_NO_TRAY").is_none() {
        use ksni::blocking::TrayMethods;
        TrayIcon(st.clone()).assume_sni_available(true).spawn().map_err(|e| eprintln!("everdir: no tray icon: {e}")).ok()
    } else {
        None
    };
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let (server, st) = (server.clone(), st.clone());
            std::thread::spawn(move || {
                for req in server.incoming_requests() {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(&st, req)));
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
}

/// Opens the UI as a Chrome/Chromium app window, falling back to the default browser. The dedicated `everdir.localhost` host gives the window its
/// own Wayland app id (`chrome-everdir.localhost__-Default`), which the .desktop file matches so
/// the taskbar shows our icon.
/// Starts the daemon if it isn't running (after Quit, or when it doesn't start at login): through
/// the systemd user unit (launchd agent on macOS) when there is one, otherwise as a detached `everdir serve`.
fn ensure_daemon() {
    if http("GET", "/api/status").is_some() {
        return;
    }
    #[cfg(target_os = "macos")]
    // SAFETY: getuid has no preconditions.
    let (mgr, args) = ("launchctl", ["kickstart".to_string(), format!("gui/{}/{LAUNCHD_LABEL}", unsafe { libc::getuid() })]);
    #[cfg(not(target_os = "macos"))]
    let (mgr, args) = ("systemctl", ["--user", "start", "everdir.service"].map(String::from).to_vec());
    let via_manager = Command::new(mgr)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    if !via_manager {
        let exe = std::env::current_exe().unwrap_or_else(|_| "everdir".into());
        let _ = Command::new(exe).arg("serve").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
    // The server answers right away and loads the cached index in the background.
    for _ in 0..50 {
        if http("GET", "/api/status").is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[cfg(target_os = "linux")]
fn autostart_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", "everdir.service"])
        .status()
        .is_ok_and(|s| s.success())
}

fn open_ui() {
    let url = format!("http://everdir.localhost:{}", port());
    let app = format!("--app={url}");
    #[cfg(target_os = "macos")]
    let (browsers, fallback, fallback_url) = (
        ["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", "/Applications/Chromium.app/Contents/MacOS/Chromium"],
        "open",
        // Safari doesn't resolve *.localhost.
        format!("http://127.0.0.1:{}", port()),
    );
    #[cfg(not(target_os = "macos"))]
    let (browsers, fallback, fallback_url) = (["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"], "xdg-open", url.clone());
    if !browsers.iter().any(|b| spawn_detached(b, &[&app])) {
        spawn_detached(fallback, &[&fallback_url]);
    }
}

#[cfg(target_os = "linux")]
struct TrayIcon(Arc<State>);

#[cfg(target_os = "linux")]
impl ksni::Tray for TrayIcon {
    fn id(&self) -> String {
        "everdir".into()
    }
    fn title(&self) -> String {
        "everdir".into()
    }
    // Pixmaps instead of a theme icon name: plasmashell caches its icon lookups, so a freshly
    // installed theme icon shows as a blank slot until the next login.
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let rgba = |size: i32, raw: &[u8]| ksni::Icon {
            width: size,
            height: size,
            data: raw.as_chunks::<4>().0.iter().flat_map(|p| [p[3], p[0], p[1], p[2]]).collect(),
        };
        vec![rgba(32, include_bytes!("../dist/tray-32.rgba")), rgba(64, include_bytes!("../dist/tray-64.rgba"))]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: "everdir".into(), description: "Search files · disk usage".into(), ..Default::default() }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        open_ui();
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::{CheckmarkItem, StandardItem};
        vec![
            StandardItem { label: "Open everdir".into(), activate: Box::new(|_: &mut Self| open_ui()), ..Default::default() }.into(),
            StandardItem { label: "Rescan now".into(), activate: Box::new(|t: &mut Self| queue_all(&t.0)), ..Default::default() }.into(),
            ksni::MenuItem::Separator,
            // Off: everdir only runs (and holds its index in memory) after you open it, until Quit.
            CheckmarkItem {
                label: "Start at login".into(),
                checked: autostart_enabled(),
                activate: Box::new(|_: &mut Self| {
                    let verb = if autostart_enabled() { "disable" } else { "enable" };
                    let _ = Command::new("systemctl").args(["--user", verb, "--quiet", "everdir.service"]).status();
                }),
                ..Default::default()
            }
            .into(),
            StandardItem { label: "Quit".into(), activate: Box::new(|_: &mut Self| std::process::exit(0)), ..Default::default() }.into(),
        ]
    }
}

fn empty_root() -> Tmp {
    Tmp { name: "/".into(), kind: DIR, size: 0, alloc: 0, mtime: 0, files: 0, children: vec![] }
}

// ---------------------------------------------------------------- CLI client

fn http(method: &str, path: &str) -> Option<String> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port())).ok()?;
    write!(s, "{method} {path} HTTP/1.0\r\nHost: 127.0.0.1:{}\r\nX-Everdir: 1\r\nContent-Length: 0\r\n\r\n", port()).ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    buf.split_once("\r\n\r\n").map(|(_, b)| b.to_string())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let url = format!("http://127.0.0.1:{}", port());
    match args.first().map(String::as_str) {
        None | Some("serve") => serve(),
        Some("open") => {
            ensure_daemon();
            open_ui();
        }
        Some("rescan") => match http("POST", "/api/rescan") {
            Some(_) => println!("rescan started"),
            None => eprintln!("everdir daemon not running"),
        },
        Some("quit") => match http("POST", "/api/quit") {
            Some(_) => println!("everdir stopped"),
            None => eprintln!("everdir daemon not running"),
        },
        Some("-h" | "--help" | "help") => {
            println!("everdir [serve]     run the daemon (UI at {url})\neverdir open        open the UI\neverdir rescan      rescan now\neverdir quit        stop the daemon and free its memory\neverdir <query...>  search from the terminal (same syntax as the UI)")
        }
        Some(_) => {
            let q = args.join(" ");
            let Some(body) = http("GET", &format!("/api/search?limit=500&q={}", pct_encode(&q, false))) else {
                eprintln!("everdir daemon not running");
                std::process::exit(1);
            };
            let v: Value = serde_json::from_str(&body).unwrap_or_default();
            let mut out = std::io::stdout().lock();
            for r in v["results"].as_array().into_iter().flatten() {
                let dir = r["dir"].as_str().unwrap_or("");
                let sep = if dir == "/" { "" } else { "/" };
                if writeln!(out, "{dir}{sep}{}", r["name"].as_str().unwrap_or("")).is_err() {
                    return;
                }
            }
            let total = v["total"].as_u64().unwrap_or(0);
            if total > 500 {
                eprintln!("… {} more", total - 500);
            }
        }
    }
}
