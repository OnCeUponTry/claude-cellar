use chrono::Utc;
use clap::{Parser, Subcommand};
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Instant, SystemTime};

const ZSTD_LEVEL: i32 = 19;
const JSONL_EXT: &str = "jsonl";
const ZST_EXT: &str = "zst";

type CompressResult = (
    std::path::PathBuf,
    std::io::Result<(std::path::PathBuf, u64, u64)>,
);

#[derive(Parser)]
#[command(
    version,
    about = "Transparent zstd compression for Claude Code sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Subcmd,
}

#[derive(Subcommand)]
enum Subcmd {
    /// Compress one .jsonl (verifies round-trip, then deletes original)
    Compress {
        file: PathBuf,
        #[arg(long)]
        keep: bool,
    },
    /// Decompress .jsonl.zst back to .jsonl (leaves .zst)
    Decompress {
        file: PathBuf,
        output: Option<PathBuf>,
    },
    /// List .jsonl and .jsonl.zst in a directory
    List { dir: PathBuf },
    /// Compress old sessions in a dir, keeping the N most recent uncompressed
    Archive {
        dir: PathBuf,
        #[arg(long, default_value_t = 5)]
        keep: usize,
        #[arg(long)]
        dry_run: bool,
    },
    /// Resume a single session by id (decompresses to scratch if needed)
    Resume {
        id: String,
        #[arg(long)]
        projects_dir: Option<PathBuf>,
        #[arg(long)]
        claude_bin: Option<PathBuf>,
    },
    /// Transparent wrapper: hydrate all compressed sessions, exec claude, re-compress on exit
    Run {
        /// Arguments forwarded verbatim to claude
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Install the claude shim (replaces ~/.local/bin/claude with a wrapper)
    Install {
        /// Explicit path to the real claude binary (auto-detected if omitted)
        #[arg(long)]
        claude_bin: Option<PathBuf>,
    },
    /// Revert install: restore the original claude binary in PATH
    Uninstall,
    /// Print the claude-cellar log
    Log {
        #[arg(long, default_value_t = 0usize)]
        tail: usize,
    },
}

fn append_ext(p: &Path, ext: &str) -> PathBuf {
    let mut s: OsString = p.as_os_str().into();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

fn strip_zst(p: &Path) -> Option<PathBuf> {
    let s = p.to_str()?;
    s.strip_suffix(".zst").map(PathBuf::from)
}

fn default_projects_dir() -> io::Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| io::Error::other("could not resolve home directory"))?;
    Ok(home.join(".claude").join("projects"))
}

fn scratch_dir() -> io::Result<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        if let Some(rt) = std::env::var_os("XDG_RUNTIME_DIR") {
            let p = PathBuf::from(rt).join("claude-cellar").join("scratch");
            fs::create_dir_all(&p)?;
            return Ok(p);
        }
    }
    let p = std::env::temp_dir().join("claude-cellar-scratch");
    fs::create_dir_all(&p)?;
    Ok(p)
}

fn config_dir() -> io::Result<PathBuf> {
    let base =
        dirs::config_dir().ok_or_else(|| io::Error::other("could not resolve config dir"))?;
    let p = base.join("claude-cellar");
    fs::create_dir_all(&p)?;
    Ok(p)
}

fn claude_bin_config_file() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("claude-bin.path"))
}

fn log_path() -> Option<PathBuf> {
    let base = dirs::state_dir().or_else(dirs::data_local_dir)?;
    Some(base.join("claude-cellar").join("cellar.log"))
}

fn sha256_reader<R: Read>(mut r: R) -> io::Result<[u8; 32]> {
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

fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    sha256_reader(BufReader::new(File::open(path)?))
}

fn sha256_zst(path: &Path) -> io::Result<[u8; 32]> {
    sha256_reader(zstd::stream::Decoder::new(BufReader::new(File::open(
        path,
    )?))?)
}

#[cfg(unix)]
fn copy_permissions(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(src)?.permissions().mode();
    let mut perm = fs::metadata(dst)?.permissions();
    perm.set_mode(mode);
    fs::set_permissions(dst, perm)
}

#[cfg(not(unix))]
fn copy_permissions(_src: &Path, _dst: &Path) -> io::Result<()> {
    Ok(())
}

fn log(level: &str, msg: &str) {
    let Some(path) = log_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let line = format!("{ts} {level:<5} {msg}\n");
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn log_info(msg: &str) {
    log("INFO", msg);
}
fn log_error(msg: &str) {
    log("ERROR", msg);
}

fn compress_file(src: &Path, keep_original: bool) -> io::Result<(PathBuf, u64, u64)> {
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

fn decompress_file(src: &Path, out: Option<&Path>) -> io::Result<PathBuf> {
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

fn list_dir(dir: &Path) -> io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name();
            let s = n.to_string_lossy();
            s.ends_with(".jsonl") || s.ends_with(".jsonl.zst")
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let meta = e.metadata()?;
        let n = e.file_name();
        let kind = if n.to_string_lossy().ends_with(".zst") {
            "zst"
        } else {
            "raw"
        };
        println!("{:>10}  {}  {}", meta.len(), kind, n.to_string_lossy());
    }
    Ok(())
}

fn collect_jsonl_sessions(root: &Path) -> io::Result<Vec<(PathBuf, SystemTime)>> {
    let mut out = Vec::new();
    walk_jsonl(root, &mut out)?;
    Ok(out)
}

fn walk_jsonl(dir: &Path, out: &mut Vec<(PathBuf, SystemTime)>) -> io::Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let ft = e.file_type()?;
        if ft.is_dir() {
            walk_jsonl(&e.path(), out)?;
        } else if ft.is_file() {
            let name = e.file_name();
            let s = name.to_string_lossy();
            if s.ends_with(".jsonl") {
                let meta = e.metadata()?;
                out.push((e.path(), meta.modified()?));
            }
        }
    }
    Ok(())
}

fn fmt_size(n: u64) -> String {
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

fn cmd_archive(dir: &Path, keep: usize, dry_run: bool) -> io::Result<()> {
    let start = Instant::now();
    let mut sessions = collect_jsonl_sessions(dir)?;
    sessions.sort_by(|a, b| b.1.cmp(&a.1));

    let total = sessions.len();
    let split = keep.min(total);
    let (hot, cold) = sessions.split_at(split);

    println!("scan: {} .jsonl in {}", total, dir.display());
    println!("HOT (keep uncompressed, {}):", hot.len());
    for (p, _) in hot {
        println!("  {}", p.file_name().unwrap_or_default().to_string_lossy());
    }
    println!("COLD (will compress, {}):", cold.len());
    for (p, _) in cold {
        println!("  {}", p.file_name().unwrap_or_default().to_string_lossy());
    }

    if dry_run {
        println!("\n[dry-run, nothing changed]");
        return Ok(());
    }
    if cold.is_empty() {
        log_info(&format!(
            "archive dir={} keep={keep} total={total} cold=0",
            dir.display()
        ));
        println!("\nnothing to compress.");
        return Ok(());
    }

    let results: Vec<CompressResult> = cold
        .par_iter()
        .map(|(p, _)| (p.clone(), compress_file(p, false)))
        .collect();

    let mut ok = 0usize;
    let mut fail = 0usize;
    let mut total_before: u64 = 0;
    let mut total_after: u64 = 0;
    println!();
    for (src, r) in results {
        match r {
            Ok((dst, before, after)) => {
                ok += 1;
                total_before += before;
                total_after += after;
                println!(
                    "  ok    {} -> {}  ({} -> {}, {:.1}%)",
                    src.file_name().unwrap_or_default().to_string_lossy(),
                    dst.file_name().unwrap_or_default().to_string_lossy(),
                    fmt_size(before),
                    fmt_size(after),
                    after as f64 * 100.0 / before as f64
                );
            }
            Err(e) => {
                fail += 1;
                eprintln!("  FAIL  {}: {}", src.display(), e);
                log_error(&format!("archive compress {}: {}", src.display(), e));
            }
        }
    }

    let saved = total_before.saturating_sub(total_after);
    let elapsed = start.elapsed();
    println!(
        "\ndone: {ok} compressed, {fail} failed, {} saved, {elapsed:.1?}",
        fmt_size(saved)
    );
    log_info(&format!(
        "archive dir={} keep={keep} total={total} cold={} ok={ok} fail={fail} saved={} elapsed={elapsed:?}",
        dir.display(),
        cold.len(),
        fmt_size(saved)
    ));
    if fail > 0 {
        return Err(io::Error::other(format!("{fail} compressions failed")));
    }
    Ok(())
}

fn find_session(projects: &Path, id: &str) -> io::Result<(PathBuf, bool)> {
    let raw_name = format!("{id}.{JSONL_EXT}");
    let zst_name = format!("{id}.{JSONL_EXT}.{ZST_EXT}");

    let direct_raw = projects.join(&raw_name);
    if direct_raw.is_file() {
        return Ok((direct_raw, false));
    }
    let direct_zst = projects.join(&zst_name);
    if direct_zst.is_file() {
        return Ok((direct_zst, true));
    }

    for entry in fs::read_dir(projects)? {
        let entry = entry?;
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        let raw = p.join(&raw_name);
        if raw.is_file() {
            return Ok((raw, false));
        }
        let zst = p.join(&zst_name);
        if zst.is_file() {
            return Ok((zst, true));
        }
    }

    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("session {id} not found under {}", projects.display()),
    ))
}

fn cmd_resume(
    id: &str,
    projects_dir: Option<PathBuf>,
    claude_bin: Option<PathBuf>,
) -> io::Result<i32> {
    let start = Instant::now();
    let projects = projects_dir.map_or_else(default_projects_dir, Ok)?;
    let (found, is_compressed) = find_session(&projects, id)?;

    let (path_to_resume, scratch_to_clean, source) = if is_compressed {
        let scratch = scratch_dir()?;
        let dst = scratch.join(format!("{id}.{JSONL_EXT}"));
        decompress_file(&found, Some(&dst))?;
        (dst.clone(), Some(dst), "zst")
    } else {
        (found.clone(), None, "raw")
    };

    let claude = claude_bin.map_or_else(resolve_claude_bin, Ok)?;

    log_info(&format!(
        "resume start id={id} source={source} found={} path={} claude={}",
        found.display(),
        path_to_resume.display(),
        claude.display()
    ));
    println!("claude-cellar: resuming {id} ({source})");

    let status = Command::new(&claude)
        .arg("--resume")
        .arg(&path_to_resume)
        .status()?;
    let exit = status.code().unwrap_or(-1);

    if let Some(p) = &scratch_to_clean
        && let Err(e) = fs::remove_file(p)
    {
        log_error(&format!("resume cleanup failed: {} ({e})", p.display()));
    }

    log_info(&format!(
        "resume end   id={id} source={source} claude_exit={exit} elapsed={:?}",
        start.elapsed()
    ));

    Ok(exit)
}

struct Hydrated {
    jsonl: PathBuf,
    size: u64,
    mtime: SystemTime,
}

fn collect_zst_without_jsonl(root: &Path) -> io::Result<Vec<(PathBuf, PathBuf)>> {
    let mut out = Vec::new();
    walk_zst(root, &mut out)?;
    Ok(out)
}

fn walk_zst(dir: &Path, out: &mut Vec<(PathBuf, PathBuf)>) -> io::Result<()> {
    for e in fs::read_dir(dir)? {
        let e = e?;
        let ft = e.file_type()?;
        if ft.is_dir() {
            walk_zst(&e.path(), out)?;
        } else if ft.is_file() {
            let name = e.file_name();
            let s = name.to_string_lossy();
            if s.ends_with(".jsonl.zst") {
                let zst = e.path();
                let Some(jsonl_str) = zst.to_string_lossy().strip_suffix(".zst").map(String::from)
                else {
                    continue;
                };
                let jsonl = PathBuf::from(jsonl_str);
                if !jsonl.exists() {
                    out.push((zst, jsonl));
                }
            }
        }
    }
    Ok(())
}

fn hydrate_parallel(dir: &Path) -> io::Result<Vec<Hydrated>> {
    let pairs = collect_zst_without_jsonl(dir)?;
    let results: Vec<io::Result<Hydrated>> = pairs
        .par_iter()
        .map(|(zst, jsonl)| {
            decompress_file(zst, Some(jsonl))?;
            let meta = fs::metadata(jsonl)?;
            Ok(Hydrated {
                jsonl: jsonl.clone(),
                size: meta.len(),
                mtime: meta.modified()?,
            })
        })
        .collect();
    let mut out = Vec::with_capacity(results.len());
    for r in results {
        out.push(r?);
    }
    Ok(out)
}

fn cleanup_hydrated_parallel(entries: &[Hydrated]) -> (usize, usize, usize) {
    // returns (unchanged_deleted, recompressed, errors)
    let counts: Vec<(usize, usize, usize)> = entries
        .par_iter()
        .map(|e| {
            if !e.jsonl.exists() {
                return (0, 0, 0);
            }
            let meta = match fs::metadata(&e.jsonl) {
                Ok(m) => m,
                Err(err) => {
                    log_error(&format!("cleanup stat {}: {err}", e.jsonl.display()));
                    return (0, 0, 1);
                }
            };
            let changed = meta.len() != e.size || meta.modified().ok().is_none_or(|t| t != e.mtime);
            if changed {
                match compress_file(&e.jsonl, false) {
                    Ok(_) => (0, 1, 0),
                    Err(err) => {
                        log_error(&format!("cleanup compress {}: {err}", e.jsonl.display()));
                        (0, 0, 1)
                    }
                }
            } else if let Err(err) = fs::remove_file(&e.jsonl) {
                log_error(&format!("cleanup rm {}: {err}", e.jsonl.display()));
                (0, 0, 1)
            } else {
                (1, 0, 0)
            }
        })
        .collect();
    counts
        .into_iter()
        .fold((0, 0, 0), |a, b| (a.0 + b.0, a.1 + b.1, a.2 + b.2))
}

fn cmd_run(claude_args: Vec<OsString>) -> io::Result<i32> {
    let start = Instant::now();
    let projects = default_projects_dir()?;
    let claude = resolve_claude_bin()?;

    let t0 = Instant::now();
    let hydrated = hydrate_parallel(&projects)?;
    let hydrate_ms = t0.elapsed().as_millis();
    log_info(&format!(
        "run hydrate dir={} count={} elapsed_ms={hydrate_ms}",
        projects.display(),
        hydrated.len()
    ));

    // Spawn child, then survive SIGINT/SIGTERM/SIGHUP so we can cleanup.
    let mut child = Command::new(&claude).args(&claude_args).spawn()?;

    #[cfg(unix)]
    let sig_handle = {
        let child_pid = child.id();
        use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
        use signal_hook::iterator::Signals;
        let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGQUIT])?;
        let handle = signals.handle();
        std::thread::spawn(move || {
            for sig in signals.forever() {
                unsafe {
                    libc::kill(child_pid as libc::pid_t, sig);
                }
            }
        });
        handle
    };

    #[cfg(windows)]
    {
        // Windows propagates Ctrl+C to all processes attached to the console.
        // Register a no-op handler so the wrapper survives and can run cleanup;
        // the child still receives the event and exits on its own.
        let _ = ctrlc::try_set_handler(|| {});
    }

    let status = child.wait()?;
    #[cfg(unix)]
    let exit = status.code().unwrap_or_else(|| {
        use std::os::unix::process::ExitStatusExt;
        status.signal().map(|s| 128 + s).unwrap_or(-1)
    });
    #[cfg(not(unix))]
    let exit = status.code().unwrap_or(-1);

    #[cfg(unix)]
    sig_handle.close();

    let t1 = Instant::now();
    let (kept_deleted, recompressed, errors) = cleanup_hydrated_parallel(&hydrated);
    let cleanup_ms = t1.elapsed().as_millis();

    log_info(&format!(
        "run end claude_exit={exit} hydrated={} recompressed={recompressed} \
         unchanged_deleted={kept_deleted} errors={errors} \
         total_elapsed={:?} hydrate_ms={hydrate_ms} cleanup_ms={cleanup_ms}",
        hydrated.len(),
        start.elapsed()
    ));

    if errors > 0 {
        eprintln!("claude-cellar: {errors} cleanup error(s); check `claude-cellar log`");
    }

    Ok(exit)
}

fn canonical_claude_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(home) = dirs::home_dir() {
        let versions_dir = home
            .join(".local")
            .join("share")
            .join("claude")
            .join("versions");
        if let Ok(rd) = fs::read_dir(&versions_dir) {
            let mut vs: Vec<_> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
            vs.sort();
            // reverse so newest last (lex sort on semver works for plain X.Y.Z)
            if let Some(latest) = vs.into_iter().last() {
                out.push(latest);
            }
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(local) = dirs::data_local_dir() {
            out.push(local.join("Programs").join("claude").join("claude.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        out.push(PathBuf::from(
            "/Applications/Claude.app/Contents/MacOS/claude",
        ));
    }
    out
}

fn read_stored_claude_bin() -> io::Result<Option<PathBuf>> {
    let p = claude_bin_config_file()?;
    if !p.is_file() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&p)?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(PathBuf::from(trimmed)))
}

fn write_stored_claude_bin(path: &Path) -> io::Result<()> {
    let p = claude_bin_config_file()?;
    fs::write(&p, format!("{}\n", path.display()))
}

fn shim_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        let name = if cfg!(windows) {
            "claude.cmd"
        } else {
            "claude"
        };
        h.join(".local").join("bin").join(name)
    })
}

fn is_our_shim(path: &Path) -> bool {
    // Heuristic: our shim is a small file whose contents include "claude-cellar"
    if let Ok(meta) = fs::metadata(path) {
        if !meta.is_file() || meta.len() > 4096 {
            return false;
        }
    } else {
        return false;
    }
    fs::read_to_string(path)
        .map(|s| s.contains("claude-cellar"))
        .unwrap_or(false)
}

fn resolve_claude_bin() -> io::Result<PathBuf> {
    if let Ok(p) = std::env::var("CLAUDE_CELLAR_CLAUDE_BIN") {
        return Ok(PathBuf::from(p));
    }
    if let Some(p) = read_stored_claude_bin()?
        && p.exists()
    {
        return Ok(p);
    }
    let self_exe = std::env::current_exe().ok();
    let shim = shim_path();
    for cand in canonical_claude_paths() {
        if !cand.exists() {
            continue;
        }
        let canon = fs::canonicalize(&cand).unwrap_or(cand.clone());
        if let Some(s) = &self_exe
            && let Ok(cs) = fs::canonicalize(s)
            && canon == cs
        {
            continue;
        }
        if let Some(s) = &shim
            && (&canon == s || &cand == s)
        {
            continue;
        }
        if is_our_shim(&cand) {
            continue;
        }
        return Ok(canon);
    }
    Err(io::Error::other(
        "claude binary not found; run `claude-cellar install` or set CLAUDE_CELLAR_CLAUDE_BIN",
    ))
}

#[cfg(unix)]
fn write_shim(shim_path: &Path, cellar_self: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let content = format!(
        "#!/usr/bin/env bash\nexec \"{}\" run -- \"$@\"\n",
        cellar_self.display()
    );
    if let Some(parent) = shim_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(shim_path, content)?;
    let mut perm = fs::metadata(shim_path)?.permissions();
    perm.set_mode(0o755);
    fs::set_permissions(shim_path, perm)?;
    Ok(())
}

#[cfg(windows)]
fn write_shim(shim_path: &Path, cellar_self: &Path) -> io::Result<()> {
    let content = format!("@echo off\r\n\"{}\" run -- %*\r\n", cellar_self.display());
    if let Some(parent) = shim_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(shim_path, content)?;
    Ok(())
}

fn cmd_install(claude_bin: Option<PathBuf>) -> io::Result<()> {
    let self_exe = std::env::current_exe()?;
    let shim = shim_path().ok_or_else(|| io::Error::other("cannot resolve shim path"))?;

    // 1. figure out real claude bin
    let real = match claude_bin {
        Some(p) => {
            if !p.exists() {
                return Err(io::Error::other(format!(
                    "provided --claude-bin does not exist: {}",
                    p.display()
                )));
            }
            fs::canonicalize(&p)?
        }
        None => {
            if shim.exists() && is_our_shim(&shim) {
                // shim already installed; rely on config
                if let Some(p) = read_stored_claude_bin()? {
                    p
                } else {
                    return Err(io::Error::other(
                        "shim already installed but no config; pass --claude-bin",
                    ));
                }
            } else if shim.exists() && shim.is_symlink() {
                let target = fs::canonicalize(&shim)?;
                if target == self_exe {
                    return Err(io::Error::other(
                        "shim is this binary already; pass --claude-bin to reinstall",
                    ));
                }
                target
            } else if shim.is_file() {
                fs::canonicalize(&shim)?
            } else {
                // last resort: canonical paths
                resolve_claude_bin()?
            }
        }
    };

    println!("real claude binary: {}", real.display());
    write_stored_claude_bin(&real)?;
    println!("saved to config: {}", claude_bin_config_file()?.display());

    // 2. replace shim
    if shim.exists() || shim.is_symlink() {
        fs::remove_file(&shim).ok();
    }
    write_shim(&shim, &self_exe)?;
    println!("shim installed at: {}", shim.display());
    log_info(&format!(
        "install shim={} real={} self_exe={}",
        shim.display(),
        real.display(),
        self_exe.display()
    ));
    println!("\nDone. `claude ...` now runs claude-cellar run transparently.");
    Ok(())
}

fn cmd_uninstall() -> io::Result<()> {
    let shim = shim_path().ok_or_else(|| io::Error::other("cannot resolve shim path"))?;
    let real = read_stored_claude_bin()?;

    if shim.exists() && is_our_shim(&shim) {
        fs::remove_file(&shim)?;
        println!("shim removed: {}", shim.display());
    } else if shim.exists() {
        println!(
            "warning: {} exists but is not our shim; not touching",
            shim.display()
        );
    }

    if let Some(real_path) = real
        && real_path.exists()
    {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_path, &shim)?;
        #[cfg(not(unix))]
        fs::copy(&real_path, &shim).map(|_| ())?;
        println!("restored {} -> {}", shim.display(), real_path.display());
    }

    log_info("uninstall done");
    Ok(())
}

fn cmd_log(tail: usize) -> io::Result<()> {
    let Some(path) = log_path() else {
        eprintln!("no log path on this platform");
        return Ok(());
    };
    if !path.is_file() {
        eprintln!("log not found: {}", path.display());
        return Ok(());
    }
    let contents = fs::read_to_string(&path)?;
    if tail == 0 {
        print!("{contents}");
    } else {
        let lines: Vec<&str> = contents.lines().collect();
        let start = lines.len().saturating_sub(tail);
        for l in &lines[start..] {
            println!("{l}");
        }
    }
    Ok(())
}

fn run(cli: Cli) -> io::Result<i32> {
    Ok(match cli.command {
        Subcmd::Compress { file, keep } => {
            let (dst, before, after) = compress_file(&file, keep)?;
            println!(
                "compressed -> {} ({} -> {}, {:.1}%)",
                dst.display(),
                fmt_size(before),
                fmt_size(after),
                after as f64 * 100.0 / before as f64
            );
            log_info(&format!(
                "compress file={} out={} before={before} after={after}",
                file.display(),
                dst.display()
            ));
            0
        }
        Subcmd::Decompress { file, output } => {
            let dst = decompress_file(&file, output.as_deref())?;
            println!("decompressed -> {}", dst.display());
            log_info(&format!(
                "decompress file={} out={}",
                file.display(),
                dst.display()
            ));
            0
        }
        Subcmd::List { dir } => {
            list_dir(&dir)?;
            0
        }
        Subcmd::Archive { dir, keep, dry_run } => {
            cmd_archive(&dir, keep, dry_run)?;
            0
        }
        Subcmd::Resume {
            id,
            projects_dir,
            claude_bin,
        } => cmd_resume(&id, projects_dir, claude_bin)?,
        Subcmd::Run { args } => cmd_run(args)?,
        Subcmd::Install { claude_bin } => {
            cmd_install(claude_bin)?;
            0
        }
        Subcmd::Uninstall => {
            cmd_uninstall()?;
            0
        }
        Subcmd::Log { tail } => {
            cmd_log(tail)?;
            0
        }
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => {
            let c = if code < 0 { 1 } else { code.min(255) };
            ExitCode::from(c as u8)
        }
        Err(e) => {
            log_error(&format!("{e}"));
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
