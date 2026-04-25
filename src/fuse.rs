//! FUSE filesystem that exposes the bundle-aware compressed store as a
//! virtual `~/.claude/projects/`. Sessions (`.jsonl` files at the top of
//! each project) appear decompressed; the sibling `<uuid>/` directories
//! that Claude creates for sub-agents and tool-results pass through
//! unchanged.
//!
//! Layout the user (claude) sees on the mount:
//!
//!   mount/<project>/<uuid>.jsonl                   ← virtual (zst-backed)
//!   mount/<project>/<uuid>/subagents/x.jsonl       ← pass-through
//!   mount/<project>/<uuid>/tool-results/y.txt      ← pass-through
//!
//! Backing store layout:
//!
//!   store/<project>/<uuid>.jsonl.zst               ← compressed
//!   store/<project>/<uuid>.jsonl.meta              ← sidecar (optional)
//!   store/<project>/<uuid>/subagents/x.jsonl       ← raw
//!   store/<project>/<uuid>/tool-results/y.txt      ← raw

use crate::store::{self, JSONL_EXT, ZST_EXT, atomic_compress_to_store, decompress_file,
    decompressed_size_of, log_error, log_info};
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(1);
const ROOT_INO: u64 = 1;

pub fn max_fds() -> usize {
    std::env::var("CLAUDE_CELLAR_MAX_FDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(16)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InodeKind {
    Root,
    /// Top-level `<project>/`, e.g. `-home-momo`. Real dir in store.
    Project,
    /// `<project>/<uuid>.jsonl` — virtual, backed by `<store>/<project>/<uuid>.jsonl.zst`.
    SessionJsonl,
    /// Pass-through directory under a project (the `<uuid>/` sibling, or any
    /// nested dir).
    PassDir,
    /// Pass-through regular file (anything inside `<uuid>/`).
    PassFile,
}

#[derive(Debug, Clone)]
struct InodeData {
    /// Path relative to the mount root.
    rel: PathBuf,
    kind: InodeKind,
}

enum FdBacking {
    /// Scratch file holding decompressed jsonl. Recompress on release(dirty).
    Virtual {
        scratch_path: PathBuf,
    },
    /// Direct file on the store. Plain close on release.
    Pass,
}

struct OpenFd {
    ino: u64,
    file: File,
    backing: FdBacking,
    dirty: bool,
}

pub struct CellarFs {
    store_dir: PathBuf,
    scratch_dir: PathBuf,
    inodes: Mutex<HashMap<u64, InodeData>>,
    rel_to_ino: Mutex<HashMap<PathBuf, u64>>,
    next_ino: AtomicU64,
    fds: Mutex<HashMap<u64, OpenFd>>,
    next_fh: AtomicU64,
    uid: u32,
    gid: u32,
}

impl CellarFs {
    pub fn new(store_dir: PathBuf, scratch_dir: PathBuf) -> Self {
        let mut inodes = HashMap::new();
        inodes.insert(
            ROOT_INO,
            InodeData {
                rel: PathBuf::new(),
                kind: InodeKind::Root,
            },
        );
        let mut rev = HashMap::new();
        rev.insert(PathBuf::new(), ROOT_INO);

        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };

        Self {
            store_dir,
            scratch_dir,
            inodes: Mutex::new(inodes),
            rel_to_ino: Mutex::new(rev),
            next_ino: AtomicU64::new(ROOT_INO + 1),
            fds: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            uid,
            gid,
        }
    }

    fn alloc_ino(&self, rel: &Path, kind: InodeKind) -> u64 {
        let mut rev = self.rel_to_ino.lock().unwrap();
        if let Some(ino) = rev.get(rel) {
            // Promote: if existing entry was different kind (rare race),
            // overwrite.
            let mut inodes = self.inodes.lock().unwrap();
            if let Some(d) = inodes.get_mut(ino) {
                d.kind = kind;
            }
            return *ino;
        }
        let ino = self.next_ino.fetch_add(1, Ordering::Relaxed);
        rev.insert(rel.to_path_buf(), ino);
        let mut inodes = self.inodes.lock().unwrap();
        inodes.insert(
            ino,
            InodeData {
                rel: rel.to_path_buf(),
                kind,
            },
        );
        ino
    }

    fn forget_ino(&self, rel: &Path) {
        let mut rev = self.rel_to_ino.lock().unwrap();
        if let Some(ino) = rev.remove(rel) {
            self.inodes.lock().unwrap().remove(&ino);
        }
    }

    fn get_inode(&self, ino: u64) -> Option<InodeData> {
        self.inodes.lock().unwrap().get(&ino).cloned()
    }

    /// Translate a session-jsonl rel ("<project>/<uuid>.jsonl") to its
    /// store-side `.jsonl.zst`.
    fn store_jsonl_zst(&self, virt_rel: &Path) -> PathBuf {
        let mut s: std::ffi::OsString = self.store_dir.join(virt_rel).into_os_string();
        s.push(".");
        s.push(ZST_EXT);
        PathBuf::from(s)
    }

    /// Pass-through translation: just store_dir + rel.
    fn store_path(&self, rel: &Path) -> PathBuf {
        self.store_dir.join(rel)
    }

    fn make_scratch_path(&self, ino: u64) -> PathBuf {
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.scratch_dir
            .join(format!("fd-{pid}-{ino}-{nanos}.{JSONL_EXT}"))
    }

    fn dir_attr(&self, ino: u64, p: &Path) -> FileAttr {
        let mtime = fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    fn root_attr(&self) -> FileAttr {
        self.dir_attr(ROOT_INO, &self.store_dir)
    }

    fn pass_file_attr(&self, ino: u64, real: &Path) -> io::Result<FileAttr> {
        let m = fs::metadata(real)?;
        let mtime = m.modified().unwrap_or(UNIX_EPOCH);
        Ok(FileAttr {
            ino,
            size: m.len(),
            blocks: m.len().div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind: FileType::RegularFile,
            perm: 0o600,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        })
    }

    fn session_jsonl_attr(&self, ino: u64, virt_rel: &Path) -> io::Result<FileAttr> {
        let zst = self.store_jsonl_zst(virt_rel);
        let m = fs::metadata(&zst)?;
        let mtime = m.modified().unwrap_or(UNIX_EPOCH);
        let size = decompressed_size_of(&zst).unwrap_or(0);
        Ok(FileAttr {
            ino,
            size,
            blocks: size.div_ceil(512),
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime: mtime,
            kind: FileType::RegularFile,
            perm: 0o600,
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        })
    }
}

fn dir_has_session_zst(dir: &Path) -> bool {
    let Ok(rd) = fs::read_dir(dir) else {
        return false;
    };
    for e in rd.flatten() {
        if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            let n = e.file_name();
            let s = n.to_string_lossy();
            if s.ends_with(".jsonl.zst") {
                return true;
            }
        }
    }
    false
}

// ── FUSE ops ────────────────────────────────────────────────────────────────

impl Filesystem for CellarFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_data) = self.get_inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name_str) = name.to_str() else {
            reply.error(libc::ENOENT);
            return;
        };

        let child_rel = parent_data.rel.join(name_str);

        match parent_data.kind {
            InodeKind::Root => {
                // Child must be a project subdir of the store.
                let dir = self.store_dir.join(name_str);
                if dir.is_dir() && dir_has_session_zst(&dir) {
                    let ino = self.alloc_ino(&child_rel, InodeKind::Project);
                    reply.entry(&TTL, &self.dir_attr(ino, &dir), 0);
                } else {
                    reply.error(libc::ENOENT);
                }
            }
            InodeKind::Project => {
                // Two cases: <stem>.jsonl (virtual) or <uuid>/ (pass-through dir).
                if name_str.ends_with(&format!(".{JSONL_EXT}")) {
                    let zst = self.store_jsonl_zst(&child_rel);
                    if zst.is_file() {
                        let ino = self.alloc_ino(&child_rel, InodeKind::SessionJsonl);
                        match self.session_jsonl_attr(ino, &child_rel) {
                            Ok(a) => reply.entry(&TTL, &a, 0),
                            Err(_) => reply.error(libc::EIO),
                        }
                    } else {
                        reply.error(libc::ENOENT);
                    }
                } else {
                    let p = self.store_path(&child_rel);
                    if p.is_dir() {
                        let ino = self.alloc_ino(&child_rel, InodeKind::PassDir);
                        reply.entry(&TTL, &self.dir_attr(ino, &p), 0);
                    } else if p.is_file() {
                        let ino = self.alloc_ino(&child_rel, InodeKind::PassFile);
                        match self.pass_file_attr(ino, &p) {
                            Ok(a) => reply.entry(&TTL, &a, 0),
                            Err(_) => reply.error(libc::EIO),
                        }
                    } else {
                        reply.error(libc::ENOENT);
                    }
                }
            }
            InodeKind::SessionJsonl | InodeKind::PassFile => reply.error(libc::ENOTDIR),
            InodeKind::PassDir => {
                let p = self.store_path(&child_rel);
                if p.is_dir() {
                    let ino = self.alloc_ino(&child_rel, InodeKind::PassDir);
                    reply.entry(&TTL, &self.dir_attr(ino, &p), 0);
                } else if p.is_file() {
                    let ino = self.alloc_ino(&child_rel, InodeKind::PassFile);
                    match self.pass_file_attr(ino, &p) {
                        Ok(a) => reply.entry(&TTL, &a, 0),
                        Err(_) => reply.error(libc::EIO),
                    }
                } else {
                    reply.error(libc::ENOENT);
                }
            }
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(data) = self.get_inode(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        match data.kind {
            InodeKind::Root => reply.attr(&TTL, &self.root_attr()),
            InodeKind::Project | InodeKind::PassDir => {
                let p = self.store_path(&data.rel);
                reply.attr(&TTL, &self.dir_attr(ino, &p));
            }
            InodeKind::SessionJsonl => match self.session_jsonl_attr(ino, &data.rel) {
                Ok(a) => reply.attr(&TTL, &a),
                Err(_) => reply.error(libc::EIO),
            },
            InodeKind::PassFile => {
                let p = self.store_path(&data.rel);
                match self.pass_file_attr(ino, &p) {
                    Ok(a) => reply.attr(&TTL, &a),
                    Err(_) => reply.error(libc::EIO),
                }
            }
        }
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let Some(data) = self.get_inode(ino) else {
            reply.error(libc::ENOENT);
            return;
        };

        let mut entries: Vec<(u64, FileType, String)> = Vec::new();
        entries.push((ino, FileType::Directory, ".".to_string()));
        entries.push((ino, FileType::Directory, "..".to_string()));

        match data.kind {
            InodeKind::Root => {
                let rd = match fs::read_dir(&self.store_dir) {
                    Ok(rd) => rd,
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    if !ft.is_dir() {
                        continue;
                    }
                    let name = e.file_name();
                    let Some(name_str) = name.to_str() else {
                        continue;
                    };
                    if !dir_has_session_zst(&e.path()) {
                        continue;
                    }
                    let rel = PathBuf::from(name_str);
                    let child = self.alloc_ino(&rel, InodeKind::Project);
                    entries.push((child, FileType::Directory, name_str.to_string()));
                }
            }
            InodeKind::Project => {
                let dir = self.store_path(&data.rel);
                let rd = match fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    let name = e.file_name();
                    let Some(name_str) = name.to_str() else {
                        continue;
                    };
                    if ft.is_file() {
                        // Skip sidecars, expose .jsonl.zst as .jsonl.
                        if let Some(stem) = name_str.strip_suffix(".jsonl.zst") {
                            let virt_name = format!("{stem}.jsonl");
                            let rel = data.rel.join(&virt_name);
                            let child = self.alloc_ino(&rel, InodeKind::SessionJsonl);
                            entries.push((child, FileType::RegularFile, virt_name));
                        }
                        // .meta sidecars and any other files in the project
                        // dir are hidden from claude.
                    } else if ft.is_dir() {
                        let rel = data.rel.join(name_str);
                        let child = self.alloc_ino(&rel, InodeKind::PassDir);
                        entries.push((child, FileType::Directory, name_str.to_string()));
                    }
                }
            }
            InodeKind::PassDir => {
                let dir = self.store_path(&data.rel);
                let rd = match fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    let name = e.file_name();
                    let Some(name_str) = name.to_str() else {
                        continue;
                    };
                    let rel = data.rel.join(name_str);
                    if ft.is_dir() {
                        let child = self.alloc_ino(&rel, InodeKind::PassDir);
                        entries.push((child, FileType::Directory, name_str.to_string()));
                    } else if ft.is_file() {
                        let child = self.alloc_ino(&rel, InodeKind::PassFile);
                        entries.push((child, FileType::RegularFile, name_str.to_string()));
                    }
                }
            }
            InodeKind::SessionJsonl | InodeKind::PassFile => {
                reply.error(libc::ENOTDIR);
                return;
            }
        }

        for (i, (ino, kind, name)) in entries.iter().enumerate().skip(offset as usize) {
            if reply.add(*ino, (i + 1) as i64, *kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request, ino: u64, flags: i32, reply: ReplyOpen) {
        let Some(data) = self.get_inode(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        {
            let fds = self.fds.lock().unwrap();
            if fds.len() >= max_fds() {
                log_error(&format!("open denied: cap reached ({})", max_fds()));
                reply.error(libc::EMFILE);
                return;
            }
        }

        let writeable = (flags & libc::O_WRONLY != 0) || (flags & libc::O_RDWR != 0);

        match data.kind {
            InodeKind::SessionJsonl => {
                let zst = self.store_jsonl_zst(&data.rel);
                let scratch = self.make_scratch_path(ino);
                if let Err(e) = decompress_file(&zst, Some(&scratch)) {
                    log_error(&format!("open jsonl decompress failed ino={ino} err={e}"));
                    reply.error(libc::EIO);
                    return;
                }
                let f = match OpenOptions::new()
                    .read(true)
                    .write(writeable)
                    .open(&scratch)
                {
                    Ok(f) => f,
                    Err(_) => {
                        let _ = fs::remove_file(&scratch);
                        reply.error(libc::EIO);
                        return;
                    }
                };
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.fds.lock().unwrap().insert(
                    fh,
                    OpenFd {
                        ino,
                        file: f,
                        backing: FdBacking::Virtual {
                            scratch_path: scratch,
                        },
                        dirty: false,
                    },
                );
                reply.opened(fh, 0);
            }
            InodeKind::PassFile => {
                let p = self.store_path(&data.rel);
                let f = match OpenOptions::new()
                    .read(true)
                    .write(writeable)
                    .append(flags & libc::O_APPEND != 0)
                    .open(&p)
                {
                    Ok(f) => f,
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.fds.lock().unwrap().insert(
                    fh,
                    OpenFd {
                        ino,
                        file: f,
                        backing: FdBacking::Pass,
                        dirty: false,
                    },
                );
                reply.opened(fh, 0);
            }
            _ => reply.error(libc::EISDIR),
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let mut fds = self.fds.lock().unwrap();
        let Some(fd) = fds.get_mut(&fh) else {
            reply.error(libc::EBADF);
            return;
        };
        let mut buf = vec![0u8; size as usize];
        match fd.file.read_at(&mut buf, offset.max(0) as u64) {
            Ok(n) => reply.data(&buf[..n]),
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn write(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let mut fds = self.fds.lock().unwrap();
        let Some(fd) = fds.get_mut(&fh) else {
            reply.error(libc::EBADF);
            return;
        };
        match fd.file.write_at(data, offset.max(0) as u64) {
            Ok(n) => {
                fd.dirty = true;
                reply.written(n as u32);
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn flush(&mut self, _req: &Request, _ino: u64, fh: u64, _lock_owner: u64, reply: ReplyEmpty) {
        if let Some(fd) = self.fds.lock().unwrap().get_mut(&fh) {
            let _ = fd.file.sync_data();
        }
        reply.ok();
    }

    fn fsync(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        if let Some(fd) = self.fds.lock().unwrap().get_mut(&fh) {
            if let Err(e) = fd.file.sync_data() {
                log_error(&format!("fsync fh={fh} err={e}"));
                reply.error(libc::EIO);
                return;
            }
        }
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let fd = self.fds.lock().unwrap().remove(&fh);
        let Some(fd) = fd else {
            reply.error(libc::EBADF);
            return;
        };
        match fd.backing {
            FdBacking::Virtual { scratch_path } => {
                let _ = fd.file.sync_data();
                drop(fd.file);
                if fd.dirty {
                    let data = match self.get_inode(fd.ino) {
                        Some(d) => d,
                        None => {
                            log_error(&format!("release virtual: unknown ino={}", fd.ino));
                            let _ = fs::remove_file(&scratch_path);
                            reply.error(libc::EIO);
                            return;
                        }
                    };
                    let zst = self.store_jsonl_zst(&data.rel);
                    if let Err(e) = atomic_compress_to_store(&scratch_path, &zst) {
                        log_error(&format!(
                            "release virtual recompress FAILED ino={} err={e}",
                            fd.ino
                        ));
                        // Keep scratch for forensics.
                        reply.error(libc::EIO);
                        return;
                    }
                    log_info(&format!("release virtual recompressed ino={}", fd.ino));
                }
                let _ = fs::remove_file(&scratch_path);
                reply.ok();
            }
            FdBacking::Pass => {
                drop(fd.file);
                reply.ok();
            }
        }
    }

    fn create(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(parent_data) = self.get_inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };

        {
            let fds = self.fds.lock().unwrap();
            if fds.len() >= max_fds() {
                reply.error(libc::EMFILE);
                return;
            }
        }

        match parent_data.kind {
            InodeKind::Project => {
                if !name_str.ends_with(&format!(".{JSONL_EXT}")) {
                    // Disallow creating arbitrary files at the project level;
                    // only sessions belong here.
                    reply.error(libc::EPERM);
                    return;
                }
                let virt_rel = parent_data.rel.join(name_str);
                let zst = self.store_jsonl_zst(&virt_rel);
                if let Some(parent_dir) = zst.parent() {
                    let _ = fs::create_dir_all(parent_dir);
                }
                let scratch = self.make_scratch_path(self.next_ino.load(Ordering::Relaxed));
                if let Err(e) = File::create(&scratch) {
                    log_error(&format!("create scratch failed: {e}"));
                    reply.error(libc::EIO);
                    return;
                }
                if let Err(e) = atomic_compress_to_store(&scratch, &zst) {
                    log_error(&format!("create initial compress failed: {e}"));
                    reply.error(libc::EIO);
                    return;
                }
                let f = match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&scratch)
                {
                    Ok(f) => f,
                    Err(_) => {
                        let _ = fs::remove_file(&scratch);
                        reply.error(libc::EIO);
                        return;
                    }
                };
                let ino = self.alloc_ino(&virt_rel, InodeKind::SessionJsonl);
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.fds.lock().unwrap().insert(
                    fh,
                    OpenFd {
                        ino,
                        file: f,
                        backing: FdBacking::Virtual {
                            scratch_path: scratch,
                        },
                        dirty: true,
                    },
                );
                let attr = match self.session_jsonl_attr(ino, &virt_rel) {
                    Ok(a) => a,
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };
                let _ = flags;
                reply.created(&TTL, &attr, 0, fh, 0);
            }
            InodeKind::PassDir => {
                let rel = parent_data.rel.join(name_str);
                let p = self.store_path(&rel);
                if let Some(parent_dir) = p.parent() {
                    let _ = fs::create_dir_all(parent_dir);
                }
                let f = match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .append(flags & libc::O_APPEND != 0)
                    .open(&p)
                {
                    Ok(f) => f,
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };
                let ino = self.alloc_ino(&rel, InodeKind::PassFile);
                let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
                self.fds.lock().unwrap().insert(
                    fh,
                    OpenFd {
                        ino,
                        file: f,
                        backing: FdBacking::Pass,
                        dirty: false,
                    },
                );
                let attr = match self.pass_file_attr(ino, &p) {
                    Ok(a) => a,
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };
                reply.created(&TTL, &attr, 0, fh, 0);
            }
            _ => reply.error(libc::EPERM),
        }
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_data) = self.get_inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        match parent_data.kind {
            InodeKind::Project => {
                let virt_rel = parent_data.rel.join(name_str);
                if name_str.ends_with(&format!(".{JSONL_EXT}")) {
                    let zst = self.store_jsonl_zst(&virt_rel);
                    match fs::remove_file(&zst) {
                        Ok(_) => {
                            store::unlink_sidecar(&zst);
                            self.forget_ino(&virt_rel);
                            reply.ok();
                        }
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {
                            reply.error(libc::ENOENT)
                        }
                        Err(_) => reply.error(libc::EIO),
                    }
                } else {
                    reply.error(libc::EPERM);
                }
            }
            InodeKind::PassDir => {
                let rel = parent_data.rel.join(name_str);
                let p = self.store_path(&rel);
                match fs::remove_file(&p) {
                    Ok(_) => {
                        self.forget_ino(&rel);
                        reply.ok();
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => reply.error(libc::ENOENT),
                    Err(_) => reply.error(libc::EIO),
                }
            }
            _ => reply.error(libc::EPERM),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let Some(parent_data) = self.get_inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let rel = parent_data.rel.join(name_str);
        let p = self.store_path(&rel);
        match parent_data.kind {
            InodeKind::Root => match fs::create_dir(&p) {
                Ok(_) => {
                    let ino = self.alloc_ino(&rel, InodeKind::Project);
                    reply.entry(&TTL, &self.dir_attr(ino, &p), 0);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => reply.error(libc::EEXIST),
                Err(_) => reply.error(libc::EIO),
            },
            InodeKind::Project | InodeKind::PassDir => {
                if let Some(parent_dir) = p.parent() {
                    let _ = fs::create_dir_all(parent_dir);
                }
                match fs::create_dir(&p) {
                    Ok(_) => {
                        let ino = self.alloc_ino(&rel, InodeKind::PassDir);
                        reply.entry(&TTL, &self.dir_attr(ino, &p), 0);
                    }
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => reply.error(libc::EEXIST),
                    Err(_) => reply.error(libc::EIO),
                }
            }
            _ => reply.error(libc::EPERM),
        }
    }

    fn rmdir(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_data) = self.get_inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let rel = parent_data.rel.join(name_str);
        let p = self.store_path(&rel);
        match fs::remove_dir(&p) {
            Ok(_) => {
                self.forget_ino(&rel);
                reply.ok();
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::EIO),
        }
    }
}

// ── Mount entry point ───────────────────────────────────────────────────────

fn ctx<T>(op: &str, r: io::Result<T>) -> io::Result<T> {
    r.map_err(|e| io::Error::new(e.kind(), format!("{op}: {e}")))
}

fn loud(msg: &str) {
    eprintln!("claude-cellar: {msg}");
    log_info(msg);
}

fn loud_err(msg: &str) {
    eprintln!("claude-cellar: ERROR: {msg}");
    log_error(msg);
}

pub fn run_mount(foreground: bool, store_dir: PathBuf, mount_dir: PathBuf) -> io::Result<()> {
    loud(&format!(
        "starting: store={} mount={} foreground={foreground} uid={} gid={}",
        store_dir.display(),
        mount_dir.display(),
        unsafe { libc::getuid() },
        unsafe { libc::getgid() },
    ));

    ctx(
        &format!("create_dir_all store_dir={}", store_dir.display()),
        fs::create_dir_all(&store_dir),
    )?;
    ctx(
        &format!("create_dir_all mount_dir={}", mount_dir.display()),
        fs::create_dir_all(&mount_dir),
    )?;
    let scratch = ctx("resolve scratch_dir", store::scratch_dir())?;

    if !foreground {
        unsafe {
            let pid = libc::fork();
            if pid < 0 {
                return Err(io::Error::last_os_error());
            }
            if pid > 0 {
                std::thread::sleep(Duration::from_millis(150));
                std::process::exit(0);
            }
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            let devnull = File::open("/dev/null")?;
            let null_fd = std::os::unix::io::AsRawFd::as_raw_fd(&devnull);
            libc::dup2(null_fd, 0);
            libc::dup2(null_fd, 1);
            libc::dup2(null_fd, 2);
            std::mem::forget(devnull);
        }
    }

    let pid_path = match store::pid_file() {
        Ok(p) => {
            let _ = fs::write(&p, format!("{}\n", std::process::id()));
            Some(p)
        }
        Err(_) => None,
    };

    let fs_impl = CellarFs::new(store_dir.clone(), scratch);
    let opts = vec![
        MountOption::FSName("claude-cellar".to_string()),
        MountOption::Subtype("claude-cellar".to_string()),
        MountOption::DefaultPermissions,
    ];

    loud(&format!("calling fuser::mount2 on {}", mount_dir.display()));
    let result = fuser::mount2(fs_impl, &mount_dir, &opts);

    if let Some(p) = pid_path {
        let _ = fs::remove_file(&p);
    }
    match &result {
        Ok(()) => loud("mount end (clean)"),
        Err(e) => loud_err(&format!("mount2 returned: {e}")),
    }
    result
}
