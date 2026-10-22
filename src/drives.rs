//! Drives (the root filesystem plus every mount of another block device), their
//! persisted settings, and the per-drive scan cache that lets the index survive restarts.

use crate::{DIR, Index, SKIP, Tmp, now};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_ROOT_EVERY: u64 = 24 * 3600;
pub const DEFAULT_OTHER_EVERY: u64 = 7 * 24 * 3600;
#[cfg(not(target_os = "macos"))]
const REAL_FS: [&str; 12] = ["btrfs", "ext4", "ext3", "ext2", "xfs", "vfat", "exfat", "ntfs", "ntfs3", "f2fs", "zfs", "bcachefs"];
#[cfg(target_os = "macos")]
const REAL_FS: [&str; 6] = ["apfs", "hfs", "msdos", "exfat", "ntfs", "ufsd_NTFS"];
#[cfg(not(target_os = "macos"))]
const NET_FS: [&str; 13] = ["cifs", "smb3", "smbfs", "nfs", "nfs4", "ceph", "glusterfs", "9p", "afs", "davfs", "fuse.sshfs", "fuse.rclone", "fuse.s3fs"];
#[cfg(target_os = "macos")]
const NET_FS: [&str; 4] = ["smbfs", "nfs", "afpfs", "webdav"];
const MAGIC: &[u8; 8] = b"EVDIR\x00\x00\x02";
/// Caches from before folders could be chosen; they always hold the whole drive.
const MAGIC_V1: &[u8; 8] = b"EVDIR\x00\x00\x01";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default())
}
fn config_file() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("everdir/drives.json")
}
fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".cache")).join("everdir")
}
fn cache_file(mp: &str) -> PathBuf {
    let hex: String = mp.bytes().map(|b| format!("{b:02x}")).collect();
    cache_dir().join(format!("drive-{hex}.bin"))
}

// ---------------------------------------------------------------- settings

#[derive(Clone)]
pub struct DriveCfg {
    pub enabled: bool,
    /// Seconds between automatic rescans; 0 means only when asked.
    pub every: u64,
    /// Folders to index instead of the whole drive (see `check_paths`); empty means all of it.
    pub paths: Vec<String>,
    /// A folder added by hand (see `check_folder`) rather than a mount.
    pub added: bool,
}

impl DriveCfg {
    pub fn default_for(mp: &str) -> DriveCfg {
        if mp == "/" {
            DriveCfg { enabled: true, every: DEFAULT_ROOT_EVERY, paths: Vec::new(), added: false }
        } else {
            DriveCfg { enabled: false, every: DEFAULT_OTHER_EVERY, paths: Vec::new(), added: false }
        }
    }
}

pub fn load_config() -> HashMap<String, DriveCfg> {
    let v: Value = fs::read_to_string(config_file()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let mut out = HashMap::new();
    for (mp, d) in v["drives"].as_object().into_iter().flatten() {
        let def = DriveCfg::default_for(mp);
        out.insert(mp.clone(), DriveCfg {
            enabled: mp == "/" || d["enabled"].as_bool().unwrap_or(def.enabled),
            every: d["every"].as_u64().unwrap_or(def.every),
            paths: d["paths"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(String::from)).collect(),
            added: mp != "/" && d["added"].as_bool().unwrap_or(false),
        });
    }
    out
}

pub fn save_config(cfg: &HashMap<String, DriveCfg>) -> std::io::Result<()> {
    let drives: serde_json::Map<String, Value> =
        cfg.iter().map(|(mp, c)| (mp.clone(), json!({ "enabled": c.enabled, "every": c.every, "paths": c.paths, "added": c.added }))).collect();
    let body = serde_json::to_string_pretty(&json!({ "drives": drives }))?;
    write_atomic(&config_file(), body.as_bytes())
}

fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, data)?;
    fs::rename(&tmp, path)
}

// ---------------------------------------------------------------- mounts

pub struct Drive {
    pub mp: String,
    pub source: String,
    pub fstype: String,
    pub disk: String,
    pub model: String,
    pub rotational: bool,
    pub dev_size: u64,
    /// A network share or an automount point: asking for its size can hang or mount it.
    pub remote: bool,
    /// A folder added by hand; `source` is the drive holding it.
    pub added: bool,
}

pub struct Mounts {
    /// "/" first, then mounts of other block devices (partitions, dm/crypt volumes), network
    /// shares and folders added by hand.
    pub drives: Vec<Drive>,
    /// Pseudo filesystems and the like; never descended into.
    pub pseudo: HashSet<PathBuf>,
}

impl Mounts {
    /// The drive holding `p`, or None when the deepest mount above it is one that is skipped.
    pub fn holder(&self, p: &Path) -> Option<&Drive> {
        let depth = |x: &Path| x.components().count();
        let d = self.drives.iter().filter(|d| p.starts_with(&d.mp)).max_by_key(|d| depth(Path::new(&d.mp)))?;
        let skipped = self.pseudo.iter().any(|x| p.starts_with(x) && depth(x) > depth(Path::new(&d.mp)));
        (!skipped).then_some(d)
    }

    /// Adds the folders added by hand as drives of their own.
    pub fn add_folders<'a>(&mut self, folders: impl Iterator<Item = &'a String>) {
        let new: Vec<Drive> = folders
            .filter(|f| !self.drives.iter().any(|d| d.mp == **f))
            .map(|f| {
                let host = self.holder(Path::new(f));
                Drive {
                    mp: f.clone(),
                    source: host.map(|h| h.mp.clone()).unwrap_or_default(),
                    fstype: host.map_or_else(|| "other".into(), |h| h.fstype.clone()),
                    disk: String::new(),
                    model: String::new(),
                    rotational: host.is_some_and(|h| h.rotational),
                    dev_size: 0,
                    remote: host.is_none_or(|h| h.remote),
                    added: true,
                }
            })
            .collect();
        self.drives.extend(new);
        self.drives.sort_by_key(|d| (d.mp != "/", d.mp.clone()));
    }
}

#[cfg(not(target_os = "macos"))]
fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 4 <= b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 4]).ok().and_then(|o| u8::from_str_radix(o, 8).ok())
        {
            out.push(v);
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(not(target_os = "macos"))]
fn sys_read(p: impl AsRef<Path>) -> String {
    fs::read_to_string(p).map(|s| s.trim().to_string()).unwrap_or_default()
}

#[cfg(not(target_os = "macos"))]
// Whole disks under a block device name, following partitions up and dm/crypt devices down to their slaves.
fn whole_disks(name: &str) -> Vec<String> {
    let sys = Path::new("/sys/class/block").join(name);
    let slaves: Vec<String> = fs::read_dir(sys.join("slaves"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    if !slaves.is_empty() {
        return slaves.iter().flat_map(|s| whole_disks(s)).collect();
    }
    if sys.join("partition").exists()
        && let Some(disk) = fs::canonicalize(&sys).ok().and_then(|r| r.parent()?.file_name().map(|n| n.to_string_lossy().into_owned()))
    {
        return vec![disk];
    }
    vec![name.to_string()]
}

#[cfg(not(target_os = "macos"))]
fn dev_name(source: &str) -> Option<String> {
    if !source.starts_with("/dev/") {
        return None;
    }
    fs::canonicalize(source).ok()?.file_name().map(|n| n.to_string_lossy().into_owned())
}

#[cfg(not(target_os = "macos"))]
pub fn mounts() -> Mounts {
    let mut pseudo: HashSet<PathBuf> = SKIP.iter().map(PathBuf::from).collect();
    let mi = fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
    // Later lines win: a path mounted over keeps only its topmost mount.
    let mut by_mp: Vec<(String, String, String)> = Vec::new();
    for line in mi.lines() {
        let Some((a, b)) = line.split_once(" - ") else { continue };
        let mut b = b.split(' ');
        let (Some(mp), Some(fst), Some(src)) = (a.split(' ').nth(4), b.next(), b.next()) else { continue };
        let mp = unescape_mount(mp);
        by_mp.retain(|m| m.0 != mp);
        by_mp.push((mp, fst.to_string(), src.to_string()));
    }
    let disks_of = |src: &str| dev_name(src).map(|n| whole_disks(&n)).unwrap_or_default();
    // Mounts of the same block device as "/" (btrfs subvolumes, bind mounts) are part of the root drive.
    let root_dev = by_mp.iter().find(|m| m.0 == "/").and_then(|m| dev_name(&m.2));
    let mut drives = Vec::new();
    for (mp, fst, src) in by_mp {
        // An automount point (x-systemd.automount, autofs maps) stays listed while its share isn't mounted.
        let remote = NET_FS.contains(&fst.as_str()) || fst == "autofs";
        if !(remote || REAL_FS.contains(&fst.as_str())) || SKIP.iter().any(|s| Path::new(&mp).starts_with(s)) {
            if mp != "/" {
                pseudo.insert(PathBuf::from(&mp));
            }
            continue;
        }
        let disks = disks_of(&src);
        let is_root = mp == "/";
        if !is_root && !remote && (root_dev.is_none() || dev_name(&src) == root_dev) {
            continue; // same block device as "/": part of the root drive
        }
        let disk = disks.first().cloned().unwrap_or_default();
        let blk = Path::new("/sys/block").join(&disk);
        drives.push(Drive {
            dev_size: dev_name(&src).map(|n| sys_read(Path::new("/sys/class/block").join(n).join("size"))).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0) * 512,
            model: sys_read(blk.join("device/model")),
            rotational: sys_read(blk.join("queue/rotational")) == "1",
            mp,
            source: src,
            fstype: fst,
            disk,
            remote,
            added: false,
        });
    }
    drives.sort_by_key(|d| (d.mp != "/", d.mp.clone()));
    Mounts { drives, pseudo }
}

#[cfg(target_os = "macos")]
pub fn mounts() -> Mounts {
    use std::ffi::CStr;
    let mut pseudo: HashSet<PathBuf> = SKIP.iter().map(PathBuf::from).collect();
    // SAFETY: getfsstat with a null buffer only returns the number of mounts.
    let n = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) }.max(0) as usize;
    // SAFETY: statfs is plain old data, so all-zero is a valid value.
    let mut buf: Vec<libc::statfs> = vec![unsafe { std::mem::zeroed() }; n + 8];
    let bytes = (buf.len() * std::mem::size_of::<libc::statfs>()) as libc::c_int;
    // SAFETY: getfsstat writes at most `bytes` bytes into `buf` and returns how many entries it filled.
    let got = unsafe { libc::getfsstat(buf.as_mut_ptr(), bytes, libc::MNT_NOWAIT) }.max(0) as usize;
    buf.truncate(got);
    // SAFETY: the kernel NUL-terminates these fixed-size name fields.
    let cstr = |b: &[libc::c_char]| unsafe { CStr::from_ptr(b.as_ptr()) }.to_string_lossy().into_owned();
    let mut drives = Vec::new();
    for m in &buf {
        let (mp, fst, src) = (cstr(&m.f_mntonname), cstr(&m.f_fstypename), cstr(&m.f_mntfromname));
        // Disk images marked hidden and the system's own volumes are never drives.
        let local = m.f_flags & libc::MNT_LOCAL as u32 != 0;
        let remote = !local && NET_FS.contains(&fst.as_str());
        let shown = m.f_flags & libc::MNT_DONTBROWSE as u32 == 0 && (remote || local && REAL_FS.contains(&fst.as_str()));
        if mp != "/" && (!shown || SKIP.iter().any(|s| Path::new(&mp).starts_with(s))) {
            pseudo.insert(PathBuf::from(&mp));
            continue;
        }
        drives.push(Drive {
            dev_size: m.f_blocks * m.f_bsize as u64,
            model: String::new(),
            rotational: false,
            disk: if remote { String::new() } else { src.trim_start_matches("/dev/").to_string() },
            mp,
            source: src,
            fstype: fst,
            remote,
            added: false,
        });
    }
    drives.sort_by_key(|d| (d.mp != "/", d.mp.clone()));
    Mounts { drives, pseudo }
}

/// Size and used bytes of the filesystem mounted at `mp`, as `df` counts them.
pub fn usage(mp: &str) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(mp).ok()?;
    // SAFETY: statvfs only writes into the zeroed struct we pass it.
    let mut v: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut v) } != 0 {
        return None;
    }
    let f = v.f_frsize as u64;
    Some((v.f_blocks as u64 * f, (v.f_blocks - v.f_bfree) as u64 * f))
}

/// `usage` for a drive that is safe to ask and is a filesystem of its own.
pub fn usage_of(d: &Drive) -> Option<(u64, u64)> {
    if d.remote || d.added { None } else { usage(&d.mp) }
}

/// Checks a folder to add as a drive of its own and returns its canonical path.
pub fn check_folder(raw: &str, m: &Mounts) -> Result<String, String> {
    let raw = raw.trim();
    let p = match raw.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => home().join(rest.trim_start_matches('/')),
        _ if raw.starts_with('/') => PathBuf::from(raw),
        _ => return Err("give the full path, starting with / or ~".into()),
    };
    let p = fs::canonicalize(&p).map_err(|e| format!("{raw}: {e}"))?;
    if !p.is_dir() {
        return Err(format!("{} is not a folder", p.display()));
    }
    if SKIP.iter().any(|s| p.starts_with(s)) {
        return Err(format!("{} is never indexed", p.display()));
    }
    let p = p.to_str().map(String::from).ok_or_else(|| format!("{} is not valid UTF-8", p.display()))?;
    if m.drives.iter().any(|d| d.mp == p) {
        return Err(format!("{p} is already in the list"));
    }
    Ok(p)
}

/// Checks the folders (newline-separated) chosen for the drive at `mp` and returns them
/// canonical, sorted, and without any that sit inside another. Relative paths start at the
/// drive's root. An empty list means the whole drive.
pub fn check_paths(mp: &str, raw: &str, m: &Mounts) -> Result<Vec<String>, String> {
    let raw: Vec<&str> = raw.split('\n').map(str::trim).filter(|r| !r.is_empty()).collect();
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    if !m.drives.iter().any(|d| d.mp == mp) {
        return Err(format!("{mp} is not mounted"));
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for r in raw {
        let p = fs::canonicalize(Path::new(mp).join(r)).map_err(|e| format!("{r}: {e}"))?;
        if !p.is_dir() {
            return Err(format!("{} is not a folder", p.display()));
        }
        if m.holder(&p).map(|d| d.mp.as_str()) != Some(mp) {
            return Err(format!("{} is not on {mp}", p.display()));
        }
        out.push(p);
    }
    if out.iter().any(|p| p == Path::new(mp)) {
        return Ok(Vec::new());
    }
    out.sort();
    out.dedup();
    let keep: Vec<&PathBuf> = out.iter().filter(|p| !out.iter().any(|o| o != *p && p.starts_with(o))).collect();
    keep.into_iter().map(|p| p.to_str().map(String::from).ok_or_else(|| format!("{} is not valid UTF-8", p.display()))).collect()
}

// ---------------------------------------------------------------- cache

#[derive(Clone, Default)]
pub struct Meta {
    pub scanned_at: u64,
    pub scan_ms: u64,
    pub errors: u64,
    pub nodes: u64,
    pub alloc: u64,
    pub files: u64,
    /// The folders the scan covered; empty for the whole drive.
    pub paths: Vec<String>,
}

fn put_var(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push(v as u8 | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn encode(t: &Tmp, out: &mut Vec<u8>, nodes: &mut u64) {
    *nodes += 1;
    out.push(t.kind);
    put_var(out, t.name.len() as u64);
    out.extend_from_slice(t.name.as_bytes());
    if t.kind == DIR {
        put_var(out, t.mtime as u64);
        put_var(out, t.children.len() as u64);
        for c in &t.children {
            encode(c, out, nodes);
        }
    } else {
        put_var(out, t.size);
        put_var(out, t.alloc);
        put_var(out, t.mtime as u64);
    }
}

struct Dec<'a>(&'a [u8], usize);

impl Dec<'_> {
    fn byte(&mut self) -> Option<u8> {
        let b = *self.0.get(self.1)?;
        self.1 += 1;
        Some(b)
    }
    fn var(&mut self) -> Option<u64> {
        let (mut v, mut s) = (0u64, 0);
        loop {
            let b = self.byte()?;
            v |= ((b & 0x7f) as u64) << s;
            if b < 0x80 {
                return Some(v);
            }
            s += 7;
            if s > 63 {
                return None;
            }
        }
    }
    // Directory sizes and file counts are not stored; they are summed back up here, exactly as a scan does.
    fn tmp(&mut self) -> Option<Tmp> {
        let kind = self.byte()?;
        let n = self.var()? as usize;
        let name: Box<str> = String::from_utf8_lossy(self.0.get(self.1..self.1 + n)?).into();
        self.1 += n;
        let mut t = Tmp { name, kind, size: 0, alloc: 0, mtime: 0, files: 0, children: Vec::new() };
        if kind == DIR {
            t.mtime = self.var()? as i64;
            let k = self.var()? as usize;
            t.children.reserve(k.min(1 << 20));
            for _ in 0..k {
                let c = self.tmp()?;
                t.size += c.size;
                t.alloc += c.alloc;
                t.files += c.files;
                t.children.push(c);
            }
        } else {
            t.size = self.var()?;
            t.alloc = self.var()?;
            t.mtime = self.var()? as i64;
            t.files = 1;
        }
        Some(t)
    }
}

/// Writes `t` out as `mp`'s cache, filling in `meta`'s node, size and file counts first.
pub fn save_cache(mp: &str, t: &Tmp, meta: &mut Meta) -> std::io::Result<()> {
    let mut body = Vec::new();
    let mut nodes = 0;
    encode(t, &mut body, &mut nodes);
    (meta.nodes, meta.alloc, meta.files) = (nodes, t.alloc, t.files as u64);
    let mut out = Vec::with_capacity(body.len() / 2);
    out.extend_from_slice(MAGIC);
    for v in [meta.scanned_at, meta.scan_ms, meta.errors, meta.nodes, meta.alloc, meta.files] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let paths = meta.paths.join("\n");
    out.extend_from_slice(&(paths.len() as u32).to_le_bytes());
    out.extend_from_slice(paths.as_bytes());
    let mut enc = lz4_flex::frame::FrameEncoder::new(out);
    enc.write_all(&body)?;
    let out = enc.finish().map_err(std::io::Error::other)?;
    write_atomic(&cache_file(mp), &out)
}

fn read_header(f: &mut impl Read) -> Option<Meta> {
    let mut h = [0u8; 8 + 6 * 8];
    f.read_exact(&mut h).ok()?;
    let v1 = &h[..8] == MAGIC_V1;
    if !v1 && &h[..8] != MAGIC {
        return None;
    }
    let v = |i: usize| u64::from_le_bytes(h[8 + i * 8..16 + i * 8].try_into().unwrap());
    let mut meta = Meta { scanned_at: v(0), scan_ms: v(1), errors: v(2), nodes: v(3), alloc: v(4), files: v(5), paths: Vec::new() };
    if !v1 {
        let mut n = [0u8; 4];
        f.read_exact(&mut n).ok()?;
        let n = u32::from_le_bytes(n) as usize;
        let mut p = Vec::new();
        f.take(n as u64).read_to_end(&mut p).ok()?;
        if p.len() != n {
            return None;
        }
        meta.paths = String::from_utf8(p).ok()?.split('\n').filter(|s| !s.is_empty()).map(String::from).collect();
    }
    Some(meta)
}

pub fn cache_meta(mp: &str) -> Option<Meta> {
    read_header(&mut fs::File::open(cache_file(mp)).ok()?)
}

pub fn load_cache(mp: &str) -> Option<(Tmp, Meta)> {
    let mut f = std::io::BufReader::new(fs::File::open(cache_file(mp)).ok()?);
    let meta = read_header(&mut f)?;
    let mut body = Vec::new();
    lz4_flex::frame::FrameDecoder::new(f).read_to_end(&mut body).ok()?;
    let t = Dec(&body, 0).tmp()?;
    Some((t, meta))
}

pub fn drop_cache(mp: &str) {
    let _ = fs::remove_file(cache_file(mp));
}

// ---------------------------------------------------------------- composing drives into one tree

/// Hangs `sub`'s contents under the directory `comps` below `t` (which is `at` on disk), adding
/// the directories on the way that `t` doesn't have and fixing up sizes.
pub fn graft(t: &mut Tmp, at: &Path, comps: &[&str], sub: Tmp) -> (u64, u64, u32) {
    let d = match comps.split_first() {
        None => {
            let d = (sub.size.wrapping_sub(t.size), sub.alloc.wrapping_sub(t.alloc), sub.files.wrapping_sub(t.files));
            t.children = sub.children;
            t.mtime = sub.mtime;
            (t.size, t.alloc, t.files) = (sub.size, sub.alloc, sub.files);
            return d;
        }
        Some((first, rest)) => {
            let at = at.join(first);
            let i = match t.children.iter().position(|c| c.kind == DIR && &*c.name == *first) {
                Some(i) => i,
                None => {
                    let mtime = fs::metadata(&at).map(|m| m.mtime()).unwrap_or(0);
                    t.children.push(Tmp { name: (*first).into(), kind: DIR, size: 0, alloc: 0, mtime, files: 0, children: Vec::new() });
                    t.children.len() - 1
                }
            };
            graft(&mut t.children[i], &at, rest, sub)
        }
    };
    t.size = t.size.wrapping_add(d.0);
    t.alloc = t.alloc.wrapping_add(d.1);
    t.files = t.files.wrapping_add(d.2);
    d
}

/// `parts` holds "/" plus other drives' trees keyed by mount point.
pub fn compose(mut parts: Vec<(String, Tmp)>) -> Tmp {
    parts.sort_by_key(|p| (p.0 != "/", p.0.matches('/').count(), p.0.clone()));
    let mut root = match parts.first() {
        Some(p) if p.0 == "/" => parts.remove(0).1,
        _ => crate::empty_root(),
    };
    root.name = "/".into();
    for (mp, t) in parts {
        let comps: Vec<&str> = mp.split('/').filter(|s| !s.is_empty()).collect();
        graft(&mut root, Path::new("/"), &comps, t);
    }
    root
}

/// Rebuilds one drive's tree from the live index (used to persist deletions without a rescan).
/// Nodes in `stop` are other drives' mount points and are written out empty.
pub fn export(ix: &Index, id: u32, stop: &HashSet<u32>) -> Tmp {
    let u = id as usize;
    let mut t = Tmp { name: ix.name(id).into(), kind: ix.kind[u], size: ix.size[u], alloc: ix.alloc[u], mtime: ix.mtime[u], files: ix.files[u], children: Vec::new() };
    if t.kind == DIR {
        (t.size, t.alloc, t.files) = (0, 0, 0);
        if !stop.contains(&id) {
            for c in ix.kids(id) {
                let k = export(ix, c, stop);
                t.size += k.size;
                t.alloc += k.alloc;
                t.files += k.files;
                t.children.push(k);
            }
        }
    }
    t
}

pub fn fresh_meta(scan_ms: u64, errors: u64) -> Meta {
    Meta { scanned_at: now(), scan_ms, errors, ..Default::default() }
}
