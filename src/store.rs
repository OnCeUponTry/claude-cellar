//! Store layout, sidecar metadata, atomic compression helpers.
//!
//! The store is a flat tree of `<sanitized-cwd>/<uuid>.jsonl.zst` files,
//! mirroring Claude Code's `~/.claude/projects/` layout. Each compressed
//! session optionally has a sidecar `<uuid>.jsonl.meta` capturing the
//! decompressed size, mtime, and SHA-256 hash of the original content.

use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub const ZSTD_LEVEL: i32 = 19;
pub const JSONL_EXT: &str = "jsonl";
pub const ZST_EXT: &str = "zst";
pub const META_EXT: &str = "meta";

// ── Path helpers ────────────────────────────────────────────────────────────

pub fn append_ext(p: &Path, ext: &str) -> PathBuf {
    let mut s: OsString = p.as_os_str().into();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

pub fn strip_zst(p: &Path) -> Option<PathBuf> {
    let s = p.to_str()?;
    s.strip_suffix(".zst").map(PathBuf::from)
}

pub fn store_dir() -> io::Result<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_CELLAR_STORE_DIR")
        && !p.is_empty()
    {
        return Ok(PathBuf::from(p));
    }
    let base =
        dirs::data_local_dir().ok_or_else(|| io::Error::other("could not resolve data dir"))?;
    Ok(base.join("claude-cellar").join("store"))
}

pub fn mount_dir() -> io::Result<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_CELLAR_MOUNT_DIR")
        && !p.is_empty()
    {
        return Ok(PathBuf::from(p));
    }
    let home =
        dirs::home_dir().ok_or_else(|| io::Error::other("could not resolve home directory"))?;
    Ok(home.join(".claude").join("projects"))
}

pub fn scratch_dir() -> io::Result<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_CELLAR_SCRATCH_DIR")
        && !p.is_empty()
    {
        let p = PathBuf::from(p);
        fs::create_dir_all(&p)?;
        return Ok(p);
    }
    if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
        let p = PathBuf::from(rt).join("claude-cellar").join("scratch");
        fs::create_dir_all(&p)?;
        return Ok(p);
    }
    let p = std::env::temp_dir().join("claude-cellar-scratch");
    fs::create_dir_all(&p)?;
    Ok(p)
}

pub fn config_dir() -> io::Result<PathBuf> {
    let base =
        dirs::config_dir().ok_or_else(|| io::Error::other("could not resolve config dir"))?;
    let p = base.join("claude-cellar");
    fs::create_dir_all(&p)?;
    Ok(p)
}

pub fn pid_file() -> io::Result<PathBuf> {
    let base = dirs::runtime_dir()
        .or_else(dirs::state_dir)
        .ok_or_else(|| io::Error::other("could not resolve runtime/state dir"))?;
    let dir = base.join("claude-cellar");
    fs::create_dir_all(&dir)?;
    Ok(dir.join("daemon.pid"))
}

pub fn log_path() -> Option<PathBuf> {
    let base = dirs::state_dir().or_else(dirs::data_local_dir)?;
    Some(base.join("claude-cellar").join("cellar.log"))
}

pub fn claude_bin_config_file() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("claude-bin.path"))
}

// ── SHA-256 ─────────────────────────────────────────────────────────────────

pub fn sha256_reader<R: Read>(mut r: R) -> io::Result<[u8; 32]> {
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = r.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

pub fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    sha256_reader(BufReader::new(File::open(path)?))
}

pub fn sha256_zst(path: &Path) -> io::Result<[u8; 32]> {
    sha256_reader(zstd::stream::Decoder::new(BufReader::new(File::open(
        path,
    )?))?)
}

// ── Permissions ─────────────────────────────────────────────────────────────

pub fn copy_permissions(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(src)?.permissions().mode();
    let mut perm = fs::metadata(dst)?.permissions();
    perm.set_mode(mode);
    fs::set_permissions(dst, perm)
}

// ── Single-file compress/decompress ─────────────────────────────────────────

/// Compress `src` to `<src>.zst`. Verifies SHA-256 round-trip before
/// optionally deleting the original. Returns (dst, original_size, compressed_size).
pub fn compress_file(src: &Path, keep_original: bool) -> io::Result<(PathBuf, u64, u64)> {
    let dst = append_ext(src, ZST_EXT);

    {
        let in_f = BufReader::new(File::open(src)?);
        let out_f = BufWriter::new(File::create(&dst)?);
        let mut enc = zstd::stream::Encoder::new(out_f, ZSTD_LEVEL)?;
        let mut reader = in_f;
        io::copy(&mut reader, &mut enc)?;
        enc.finish()?;
    }

    copy_permissions(src, &dst)?;

    let src_hash = sha256_file(src)?;
    let zst_hash = sha256_zst(&dst)?;
    if src_hash != zst_hash {
        let _ = fs::remove_file(&dst);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "verify failed: round-trip hash mismatch (zst removed, original kept)",
        ));
    }

    let orig_size = fs::metadata(src)?.len();
    let new_size = fs::metadata(&dst)?.len();

    if !keep_original {
        fs::remove_file(src)?;
    }

    Ok((dst, orig_size, new_size))
}

pub fn decompress_file(src: &Path, out: Option<&Path>) -> io::Result<PathBuf> {
    let dst = match out {
        Some(p) => p.to_path_buf(),
        None => strip_zst(src)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "input must end in .zst"))?,
    };
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }
    let in_f = BufReader::new(File::open(src)?);
    let out_f = BufWriter::new(File::create(&dst)?);
    let mut dec = zstd::stream::Decoder::new(in_f)?;
    let mut writer = out_f;
    io::copy(&mut dec, &mut writer)?;
    copy_permissions(src, &dst).ok();
    Ok(dst)
}

// ── Sidecar ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Sidecar {
    pub decompressed_size: u64,
    pub mtime_secs: i64,
    pub sha256: [u8; 32],
}

pub fn sidecar_path(zst_path: &Path) -> PathBuf {
    // strip ".zst" then append ".meta"
    if let Some(no_zst) = strip_zst(zst_path) {
        append_ext(&no_zst, META_EXT)
    } else {
        append_ext(zst_path, META_EXT)
    }
}

pub fn write_sidecar(zst_path: &Path, sc: &Sidecar) -> io::Result<()> {
    let p = sidecar_path(zst_path);
    let mut buf = Vec::with_capacity(8 + 8 + 32);
    buf.extend_from_slice(&sc.decompressed_size.to_le_bytes());
    buf.extend_from_slice(&sc.mtime_secs.to_le_bytes());
    buf.extend_from_slice(&sc.sha256);
    let tmp = append_ext(&p, "tmp");
    fs::write(&tmp, &buf)?;
    fs::rename(&tmp, &p)?;
    Ok(())
}

pub fn read_sidecar(zst_path: &Path) -> io::Result<Sidecar> {
    let p = sidecar_path(zst_path);
    let buf = fs::read(&p)?;
    if buf.len() != 8 + 8 + 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sidecar length mismatch",
        ));
    }
    let mut size_b = [0u8; 8];
    size_b.copy_from_slice(&buf[0..8]);
    let mut mtime_b = [0u8; 8];
    mtime_b.copy_from_slice(&buf[8..16]);
    let mut sha = [0u8; 32];
    sha.copy_from_slice(&buf[16..48]);
    Ok(Sidecar {
        decompressed_size: u64::from_le_bytes(size_b),
        mtime_secs: i64::from_le_bytes(mtime_b),
        sha256: sha,
    })
}

pub fn unlink_sidecar(zst_path: &Path) {
    let _ = fs::remove_file(sidecar_path(zst_path));
}

/// Get the decompressed size of a .zst, preferring sidecar; falls back to
/// reading the zstd frame's content-size header.
pub fn decompressed_size_of(zst_path: &Path) -> io::Result<u64> {
    if let Ok(sc) = read_sidecar(zst_path) {
        return Ok(sc.decompressed_size);
    }
    // zstd frame header carries content size when --content-size was used
    // (the default in the rust crate). Read first 18 bytes is enough for the
    // header parser, but use the public helper.
    let mut f = File::open(zst_path)?;
    let mut header = [0u8; 18];
    let _ = f.read(&mut header)?;
    let size = zstd::bulk::Decompressor::upper_bound(&header).unwrap_or(0);
    if size > 0 {
        return Ok(size as u64);
    }
    // Last resort: full decode to count bytes. Slow but correct.
    let mut count = 0u64;
    let mut dec = zstd::stream::Decoder::new(BufReader::new(File::open(zst_path)?))?;
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = dec.read(&mut buf)?;
        if n == 0 {
            break;
        }
        count += n as u64;
    }
    Ok(count)
}

/// Atomically compress `scratch_jsonl` to `store_zst` (and write its sidecar).
/// Writes to `<store_zst>.tmp`, verifies SHA-256 round-trip, then renames
/// over `store_zst`. The original `.zst` (if any) is replaced atomically.
pub fn atomic_compress_to_store(scratch_jsonl: &Path, store_zst: &Path) -> io::Result<u64> {
    if let Some(parent) = store_zst.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = append_ext(store_zst, "tmp");
    {
        let in_f = BufReader::new(File::open(scratch_jsonl)?);
        let out_f = BufWriter::new(File::create(&tmp)?);
        let mut enc = zstd::stream::Encoder::new(out_f, ZSTD_LEVEL)?;
        let mut reader = in_f;
        io::copy(&mut reader, &mut enc)?;
        enc.finish()?;
    }
    let src_hash = sha256_file(scratch_jsonl)?;
    let zst_hash = sha256_zst(&tmp)?;
    if src_hash != zst_hash {
        let _ = fs::remove_file(&tmp);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "verify failed: round-trip hash mismatch (tmp removed)",
        ));
    }
    let size = fs::metadata(scratch_jsonl)?.len();
    let mtime = fs::metadata(scratch_jsonl)?
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    fs::rename(&tmp, store_zst)?;
    let sc = Sidecar {
        decompressed_size: size,
        mtime_secs: mtime,
        sha256: src_hash,
    };
    let _ = write_sidecar(store_zst, &sc);
    Ok(size)
}

// ── Logging ─────────────────────────────────────────────────────────────────

pub fn log(level: &str, msg: &str) {
    let Some(path) = log_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let line = format!("{ts} {level:<5} {msg}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

pub fn log_info(msg: &str) {
    log("INFO", msg);
}
pub fn log_error(msg: &str) {
    log("ERROR", msg);
}

// ── Misc ────────────────────────────────────────────────────────────────────

pub fn fmt_size(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if n >= GB {
        format!("{:.1} GB", n as f64 / GB as f64)
    } else if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{n} B")
    }
}

