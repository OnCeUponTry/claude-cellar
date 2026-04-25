//! FUSE filesystem that exposes the compressed store as virtual `.jsonl`
//! sessions to Claude Code.
//!
//! Layout from Claude's perspective (mounted on `~/.claude/projects/`):
//!
//!   /
//!   ├── -home-momo/                 (project, dir)
//!   │   ├── abc-def.jsonl           (session, decompressed-size)
//!   │   └── ghi-jkl.jsonl
//!   └── -home-momo-repos-x/
//!       └── ...
//!
//! Backing store layout (e.g. `~/.local/share/claude-cellar/store/`):
//!
//!   /
//!   ├── -home-momo/
//!   │   ├── abc-def.jsonl.zst
//!   │   ├── abc-def.jsonl.meta      (sidecar)
//!   │   └── ghi-jkl.jsonl.zst
//!   └── ...
//!
//! Per-FD scratch buffers live in `$XDG_RUNTIME_DIR/claude-cellar/scratch/`
//! and are decompressed lazily on `open` and recompressed on `release` if
//! the FD was dirtied by `write`.

use crate::store::{
    self, JSONL_EXT, ZST_EXT, atomic_compress_to_store, decompress_file, decompressed_size_of,
    log_error, log_info,
};
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
    Project,
    Session,
}

#[derive(Debug, Clone)]
struct InodeData {
    /// Path relative to store root. For root this is "". For project,
    /// the project dirname. For session, "<project>/<uuid>.jsonl"
    /// (the *virtual* name without `.zst`).
    rel: PathBuf,
    kind: InodeKind,
}

struct OpenFd {
    ino: u64,
    /// Absolute scratch path (a decompressed jsonl living in tmpfs).
    scratch_path: PathBuf,
    /// Open scratch file, used for read/write/fsync.
    scratch_file: File,
    /// True after any successful write.
    dirty: bool,
}

pub struct CellarFs {
    store_dir: PathBuf,
    scratch_dir: PathBuf,
    /// Inode → metadata.
    inodes: Mutex<HashMap<u64, InodeData>>,
    /// Reverse: relative path → inode. Used to reuse inodes across lookups.
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

    fn get_inode(&self, ino: u64) -> Option<InodeData> {
        self.inodes.lock().unwrap().get(&ino).cloned()
    }

    /// Translate a virtual session path ("project/uuid.jsonl") to its
    /// store-side `.zst`.
    fn store_zst(&self, virtual_rel: &Path) -> PathBuf {
        let mut p = self.store_dir.join(virtual_rel);
        let mut s: std::ffi::OsString = p.into_os_string();
        s.push(".");
        s.push(ZST_EXT);
        p = PathBuf::from(s);
        p
    }

    fn store_dir_path(&self, virtual_rel: &Path) -> PathBuf {
        self.store_dir.join(virtual_rel)
    }

    fn root_attr(&self) -> FileAttr {
        let mtime = fs::metadata(&self.store_dir)
            .and_then(|m| m.modified())
            .unwrap_or(UNIX_EPOCH);
        FileAttr {
            ino: ROOT_INO,
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

    fn project_attr(&self, ino: u64, rel: &Path) -> FileAttr {
        let dir = self.store_dir_path(rel);
        let mtime = fs::metadata(&dir)
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

    fn session_attr(&self, ino: u64, rel: &Path) -> io::Result<FileAttr> {
        let zst = self.store_zst(rel);
        let zst_meta = fs::metadata(&zst)?;
        let mtime = zst_meta.modified().unwrap_or(UNIX_EPOCH);
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

    fn make_scratch_path(&self, ino: u64) -> PathBuf {
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        self.scratch_dir
            .join(format!("fd-{pid}-{ino}-{nanos}.{JSONL_EXT}"))
    }
}

// ── FUSE ops ────────────────────────────────────────────────────────────────

impl Filesystem for CellarFs {
    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(parent_data) = self.get_inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };

        match parent_data.kind {
            InodeKind::Root => {
                let dir = self.store_dir.join(name_str);
                if dir.is_dir() {
                    let rel = PathBuf::from(name_str);
                    let ino = self.alloc_ino(&rel, InodeKind::Project);
                    reply.entry(&TTL, &self.project_attr(ino, &rel), 0);
                } else {
                    reply.error(libc::ENOENT);
                }
            }
            InodeKind::Project => {
                if !name_str.ends_with(".jsonl") {
                    reply.error(libc::ENOENT);
                    return;
                }
                let virt_rel = parent_data.rel.join(name_str);
                let zst = self.store_zst(&virt_rel);
                if zst.is_file() {
                    let ino = self.alloc_ino(&virt_rel, InodeKind::Session);
                    match self.session_attr(ino, &virt_rel) {
                        Ok(attr) => reply.entry(&TTL, &attr, 0),
                        Err(_) => reply.error(libc::EIO),
                    }
                } else {
                    reply.error(libc::ENOENT);
                }
            }
            InodeKind::Session => reply.error(libc::ENOTDIR),
        }
    }

    fn getattr(&mut self, _req: &Request, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        let Some(data) = self.get_inode(ino) else {
            reply.error(libc::ENOENT);
            return;
        };
        match data.kind {
            InodeKind::Root => reply.attr(&TTL, &self.root_attr()),
            InodeKind::Project => reply.attr(&TTL, &self.project_attr(ino, &data.rel)),
            InodeKind::Session => match self.session_attr(ino, &data.rel) {
                Ok(attr) => reply.attr(&TTL, &attr),
                Err(_) => reply.error(libc::EIO),
            },
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
                    let rel = PathBuf::from(name_str);
                    let child_ino = self.alloc_ino(&rel, InodeKind::Project);
                    entries.push((child_ino, FileType::Directory, name_str.to_string()));
                }
            }
            InodeKind::Project => {
                let dir = self.store_dir_path(&data.rel);
                let rd = match fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(_) => {
                        reply.error(libc::EIO);
                        return;
                    }
                };
                for e in rd.flatten() {
                    let Ok(ft) = e.file_type() else { continue };
                    if !ft.is_file() {
                        continue;
                    }
                    let name = e.file_name();
                    let Some(name_str) = name.to_str() else {
                        continue;
                    };
                    let Some(jsonl_name) = name_str.strip_suffix(".zst") else {
                        continue;
                    };
                    if !jsonl_name.ends_with(".jsonl") {
                        continue;
                    }
                    let virt_rel = data.rel.join(jsonl_name);
                    let child_ino = self.alloc_ino(&virt_rel, InodeKind::Session);
                    entries.push((
                        child_ino,
                        FileType::RegularFile,
                        jsonl_name.to_string(),
                    ));
                }
            }
            InodeKind::Session => {
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
        if data.kind != InodeKind::Session {
            reply.error(libc::EISDIR);
            return;
        }

        // FD cap
        {
            let fds = self.fds.lock().unwrap();
            if fds.len() >= max_fds() {
                log_error(&format!(
                    "open denied: cap reached ({}); ino={ino}",
                    max_fds()
                ));
                reply.error(libc::EMFILE);
                return;
            }
        }

        let zst = self.store_zst(&data.rel);
        let scratch = self.make_scratch_path(ino);
        if let Err(e) = decompress_file(&zst, Some(&scratch)) {
            log_error(&format!(
                "open decompress failed ino={ino} zst={} err={e}",
                zst.display()
            ));
            reply.error(libc::EIO);
            return;
        }

        let scratch_file = match OpenOptions::new()
            .read(true)
            .write((flags & libc::O_WRONLY != 0) || (flags & libc::O_RDWR != 0))
            .open(&scratch)
        {
            Ok(f) => f,
            Err(e) => {
                log_error(&format!(
                    "open scratch failed ino={ino} scratch={} err={e}",
                    scratch.display()
                ));
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
                scratch_path: scratch,
                scratch_file,
                dirty: false,
            },
        );
        log_info(&format!("open ok ino={ino} fh={fh}"));
        reply.opened(fh, 0);
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
        match fd.scratch_file.read_at(&mut buf, offset.max(0) as u64) {
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
        match fd.scratch_file.write_at(data, offset.max(0) as u64) {
            Ok(n) => {
                fd.dirty = true;
                reply.written(n as u32);
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn flush(
        &mut self,
        _req: &Request,
        _ino: u64,
        fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        let mut fds = self.fds.lock().unwrap();
        if let Some(fd) = fds.get_mut(&fh) {
            let _ = fd.scratch_file.sync_data();
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
        let mut fds = self.fds.lock().unwrap();
        if let Some(fd) = fds.get_mut(&fh) {
            if let Err(e) = fd.scratch_file.sync_data() {
                log_error(&format!("fsync failed fh={fh} err={e}"));
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

        if fd.dirty {
            // Recompress to store atomically.
            let data = match self.get_inode(fd.ino) {
                Some(d) => d,
                None => {
                    log_error(&format!("release: unknown ino={}", fd.ino));
                    let _ = fs::remove_file(&fd.scratch_path);
                    reply.error(libc::EIO);
                    return;
                }
            };
            let store_zst = self.store_zst(&data.rel);
            // sync_data before reading back for compression
            let _ = fd.scratch_file.sync_data();
            drop(fd.scratch_file);
            match atomic_compress_to_store(&fd.scratch_path, &store_zst) {
                Ok(size) => {
                    log_info(&format!(
                        "release recompressed ino={} size={size}",
                        fd.ino
                    ));
                }
                Err(e) => {
                    // Keep scratch for forensics; user can find it in $SCRATCH.
                    log_error(&format!(
                        "release recompress FAILED ino={} scratch={} err={e}",
                        fd.ino,
                        fd.scratch_path.display()
                    ));
                    reply.error(libc::EIO);
                    return;
                }
            }
        } else {
            drop(fd.scratch_file);
        }

        let _ = fs::remove_file(&fd.scratch_path);
        reply.ok();
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
        if parent_data.kind != InodeKind::Project {
            // We don't allow creating projects via create(); claude does
            // mkdir() for that.
            reply.error(libc::EPERM);
            return;
        }
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        if !name_str.ends_with(".jsonl") {
            reply.error(libc::EINVAL);
            return;
        }

        // FD cap
        {
            let fds = self.fds.lock().unwrap();
            if fds.len() >= max_fds() {
                reply.error(libc::EMFILE);
                return;
            }
        }

        let virt_rel = parent_data.rel.join(name_str);
        // Materialize an empty zst+sidecar so subsequent stat works.
        let store_zst = self.store_zst(&virt_rel);
        if let Some(parent_dir) = store_zst.parent() {
            let _ = fs::create_dir_all(parent_dir);
        }

        // Create empty scratch file. We don't write the zst until
        // release(dirty), which keeps create cheap.
        let ino = self.alloc_ino(&virt_rel, InodeKind::Session);
        let scratch = self.make_scratch_path(ino);
        if let Err(e) = File::create(&scratch) {
            log_error(&format!(
                "create: failed to make scratch {} err={e}",
                scratch.display()
            ));
            reply.error(libc::EIO);
            return;
        }

        // For consistency, also write an empty .zst so getattr (if it's
        // called before any release) doesn't ENOENT.
        let _ = atomic_compress_to_store(&scratch, &store_zst);

        let scratch_file = match OpenOptions::new()
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

        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.fds.lock().unwrap().insert(
            fh,
            OpenFd {
                ino,
                scratch_path: scratch,
                scratch_file,
                // Mark dirty so an immediate close re-syncs the (empty) zst.
                dirty: true,
            },
        );

        let attr = match self.session_attr(ino, &virt_rel) {
            Ok(a) => a,
            Err(_) => {
                reply.error(libc::EIO);
                return;
            }
        };
        let _ = flags;
        reply.created(&TTL, &attr, 0, fh, 0);
    }

    fn unlink(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(parent_data) = self.get_inode(parent) else {
            reply.error(libc::ENOENT);
            return;
        };
        if parent_data.kind != InodeKind::Project {
            reply.error(libc::EPERM);
            return;
        }
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let virt_rel = parent_data.rel.join(name_str);
        let zst = self.store_zst(&virt_rel);
        match fs::remove_file(&zst) {
            Ok(_) => {
                store::unlink_sidecar(&zst);
                // Drop our cached inode/path so future lookups ENOENT.
                let mut rev = self.rel_to_ino.lock().unwrap();
                if let Some(ino) = rev.remove(&virt_rel) {
                    self.inodes.lock().unwrap().remove(&ino);
                }
                reply.ok();
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => reply.error(libc::ENOENT),
            Err(_) => reply.error(libc::EIO),
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
        if parent_data.kind != InodeKind::Root {
            reply.error(libc::EPERM);
            return;
        }
        let Some(name_str) = name.to_str() else {
            reply.error(libc::EINVAL);
            return;
        };
        let dir = self.store_dir.join(name_str);
        match fs::create_dir(&dir) {
            Ok(_) => {
                let rel = PathBuf::from(name_str);
                let ino = self.alloc_ino(&rel, InodeKind::Project);
                reply.entry(&TTL, &self.project_attr(ino, &rel), 0);
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => reply.error(libc::EEXIST),
            Err(_) => reply.error(libc::EIO),
        }
    }
}

// ── Mount entry point ───────────────────────────────────────────────────────

/// Wrap an io error with a contextual prefix so we know which path/operation
/// blew up. The bare `io::Error` from std doesn't include the path that
/// failed, which makes diagnostics like "ENOENT" useless.
fn ctx<T>(op: &str, r: io::Result<T>) -> io::Result<T> {
    r.map_err(|e| io::Error::new(e.kind(), format!("{op}: {e}")))
}

/// Print to stderr AND log; useful for systemd where stderr lands in journal.
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
        "starting: store={} mount={} foreground={foreground} \
         uid={} gid={} XDG_RUNTIME_DIR={:?} HOME={:?}",
        store_dir.display(),
        mount_dir.display(),
        unsafe { libc::getuid() },
        unsafe { libc::getgid() },
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::var_os("HOME"),
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
    loud(&format!("scratch ok: {}", scratch.display()));

    if !foreground {
        // Daemonize: fork, child runs in new session detached from terminal.
        unsafe {
            let pid = libc::fork();
            if pid < 0 {
                let e = io::Error::last_os_error();
                loud_err(&format!("fork failed: {e}"));
                return Err(e);
            }
            if pid > 0 {
                // Parent: wait briefly for child to finish setup, then exit.
                std::thread::sleep(Duration::from_millis(150));
                std::process::exit(0);
            }
            // Child: detach.
            if libc::setsid() < 0 {
                let e = io::Error::last_os_error();
                loud_err(&format!("setsid failed: {e}"));
                return Err(e);
            }
            // Redirect stdio to /dev/null.
            let devnull = ctx("open /dev/null", File::open("/dev/null"))?;
            let null_fd = std::os::unix::io::AsRawFd::as_raw_fd(&devnull);
            libc::dup2(null_fd, 0);
            libc::dup2(null_fd, 1);
            libc::dup2(null_fd, 2);
            std::mem::forget(devnull);
        }
    }

    // Write PID file. Non-fatal if it fails — the daemon still works.
    let pid_path = match store::pid_file() {
        Ok(p) => {
            if let Err(e) = fs::write(&p, format!("{}\n", std::process::id())) {
                loud_err(&format!("pidfile write {} failed: {e} (continuing)", p.display()));
            }
            Some(p)
        }
        Err(e) => {
            loud_err(&format!("pidfile resolve failed: {e} (continuing without pidfile)"));
            None
        }
    };

    let fs_impl = CellarFs::new(store_dir.clone(), scratch.clone());
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

