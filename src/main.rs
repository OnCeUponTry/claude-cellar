#[cfg(not(target_os = "linux"))]
compile_error!("claude-cellar requires Linux (it depends on FUSE and fusermount3).");

mod fuse;
mod store;

use clap::{Parser, Subcommand};
use rayon::prelude::*;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Instant, SystemTime};

use store::{
    JSONL_EXT, ZST_EXT, append_ext, atomic_compress_to_store, claude_bin_config_file,
    compress_file, config_dir, decompress_file, fmt_size, log_error, log_info, log_path,
    mount_dir, scratch_dir, sidecar_path, store_dir,
};

type CompressResult = (PathBuf, io::Result<(PathBuf, u64, u64)>);

#[derive(Parser)]
#[command(version, about = "Bundle-aware zstd compression for Claude Code sessions (Linux/FUSE)")]
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
    /// Mount the FUSE filesystem (default: fork-and-exit; --foreground for systemd)
    Mount {
        #[arg(long)]
        foreground: bool,
        #[arg(long)]
        store_dir: Option<PathBuf>,
        #[arg(long)]
        mount_dir: Option<PathBuf>,
    },
    /// Unmount the FUSE filesystem
    Umount {
        #[arg(long)]
        mount_dir: Option<PathBuf>,
    },
    /// Print mount/store status
    Status,
    /// Migrate existing sessions (and their bundle dirs) into the store layout
    MigrateStore {
        #[arg(long, default_value = "~/.claude/projects")]
        from: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Install: register and start the systemd user service. Auto-migrates
    /// pre-existing sessions (with their sub-agent / tool-result bundle dirs).
    Install {
        #[arg(long)]
        no_systemd: bool,
        #[arg(long)]
        claude_bin: Option<PathBuf>,
    },
    /// Uninstall: stop the systemd service. Store is preserved.
    Uninstall,
    /// Print the claude-cellar log
    Log {
        #[arg(long, default_value_t = 0usize)]
        tail: usize,
    },
}

// ── Mount detection ─────────────────────────────────────────────────────────

fn is_path_mounted_with_cellar(path: &Path) -> bool {
    let canon = match fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let target = canon.to_string_lossy().into_owned();
    let Ok(f) = File::open("/proc/self/mountinfo") else {
        return false;
    };
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let mut it = line.split_whitespace();
        let _ = it.next();
        let _ = it.next();
        let _ = it.next();
        let _ = it.next();
        let mp = match it.next() {
            Some(s) => s,
            None => continue,
        };
        if mp != target {
            continue;
        }
        let mut found_sep = false;
        for tok in it.by_ref() {
            if tok == "-" {
                found_sep = true;
                break;
            }
        }
        if !found_sep {
            continue;
        }
        let fstype = it.next().unwrap_or("");
        if fstype.starts_with("fuse") && line.contains("claude-cellar") {
            return true;
        }
    }
    false
}

// ── Single-file commands ────────────────────────────────────────────────────

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

// ── Archive ─────────────────────────────────────────────────────────────────

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

fn cmd_archive(dir: &Path, keep: usize, dry_run: bool) -> io::Result<()> {
    if is_path_mounted_with_cellar(dir) {
        return Err(io::Error::other(format!(
            "{} is mounted by claude-cellar; stop the daemon first (claude-cellar umount).",
            dir.display()
        )));
    }
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
            }
        }
    }
    let saved = total_before.saturating_sub(total_after);
    let elapsed = start.elapsed();
    println!(
        "\ndone: {ok} compressed, {fail} failed, {} saved, {elapsed:.1?}",
        fmt_size(saved)
    );
    if fail > 0 {
        return Err(io::Error::other(format!("{fail} compressions failed")));
    }
    Ok(())
}

// ── Resume ──────────────────────────────────────────────────────────────────

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

fn cmd_resume(id: &str, projects_dir: Option<PathBuf>, claude_bin: Option<PathBuf>) -> io::Result<i32> {
    let projects = projects_dir.map_or_else(default_projects_dir, Ok)?;
    let (found, is_compressed) = find_session(&projects, id)?;
    let (path_to_resume, scratch_to_clean) = if is_compressed {
        let scratch = scratch_dir()?;
        let dst = scratch.join(format!("{id}.{JSONL_EXT}"));
        decompress_file(&found, Some(&dst))?;
        (dst.clone(), Some(dst))
    } else {
        (found.clone(), None)
    };
    let claude = claude_bin.map_or_else(resolve_claude_bin, Ok)?;
    println!(
        "claude-cellar: resuming {id} ({})",
        if is_compressed { "zst" } else { "raw" }
    );
    let status = Command::new(&claude)
        .arg("--resume")
        .arg(&path_to_resume)
        .status()?;
    let exit = status.code().unwrap_or(-1);
    if let Some(p) = &scratch_to_clean {
        let _ = fs::remove_file(p);
    }
    Ok(exit)
}

// ── Mount/umount/status ─────────────────────────────────────────────────────

fn cmd_mount(
    foreground: bool,
    store_dir_arg: Option<PathBuf>,
    mount_dir_arg: Option<PathBuf>,
) -> io::Result<()> {
    let store = store_dir_arg.map_or_else(store_dir, Ok)?;
    let mount = mount_dir_arg.map_or_else(mount_dir, Ok)?;
    if is_path_mounted_with_cellar(&mount) {
        if foreground {
            return Err(io::Error::other(format!(
                "{} already has a claude-cellar mount; refusing.",
                mount.display()
            )));
        }
        eprintln!("claude-cellar: already mounted at {}", mount.display());
        return Ok(());
    }
    fuse::run_mount(foreground, store, mount)
}

fn cmd_umount(mount_dir_arg: Option<PathBuf>) -> io::Result<()> {
    let mount = mount_dir_arg.map_or_else(mount_dir, Ok)?;
    if !is_path_mounted_with_cellar(&mount) {
        println!("claude-cellar: nothing to unmount at {}", mount.display());
        return Ok(());
    }
    let status = Command::new("fusermount3").arg("-u").arg(&mount).status()?;
    if !status.success() {
        return Err(io::Error::other("fusermount3 -u failed"));
    }
    println!("claude-cellar: unmounted {}", mount.display());
    Ok(())
}

fn cmd_status() -> io::Result<()> {
    let store = store_dir()?;
    let mount = mount_dir()?;
    println!("store:  {}", store.display());
    println!("mount:  {}", mount.display());
    let mounted = is_path_mounted_with_cellar(&mount);
    println!("active: {}", if mounted { "yes" } else { "no" });
    if let Ok(pid) = fs::read_to_string(store::pid_file()?) {
        println!("pid:    {}", pid.trim());
    }
    if store.is_dir() {
        let mut count = 0u64;
        let mut bytes = 0u64;
        if let Ok(rd) = fs::read_dir(&store) {
            for proj in rd.flatten() {
                if proj.file_type().map(|t| t.is_dir()).unwrap_or(false)
                    && let Ok(rd2) = fs::read_dir(proj.path())
                {
                    for e in rd2.flatten() {
                        if let Ok(m) = e.metadata()
                            && m.is_file()
                            && e.file_name().to_string_lossy().ends_with(".zst")
                        {
                            count += 1;
                            bytes += m.len();
                        }
                    }
                }
            }
        }
        println!(
            "store contents: {count} sessions, {} compressed",
            fmt_size(bytes)
        );
    }
    Ok(())
}

// ── Migrate (bundle-aware) ──────────────────────────────────────────────────

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(s)
}

#[derive(Debug, PartialEq, Eq)]
enum Layout {
    Empty,
    /// Sessions live as `<uuid>.jsonl` (raw or .zst) at the top, optionally
    /// with sibling `<uuid>/` bundle dirs. The classic NFS-shared layout
    /// after symlink resolution.
    Flat,
    /// Top-level subdirs are full Claude project directories.
    Projected,
}

fn detect_layout(dir: &Path) -> io::Result<Layout> {
    let mut top_jsonl = false;
    let mut subdir_jsonl = false;
    for e in fs::read_dir(dir)?.flatten() {
        let ft = match e.file_type() {
            Ok(t) => t,
            Err(_) => continue,
        };
        if ft.is_file() {
            let s = e.file_name().to_string_lossy().into_owned();
            if s.ends_with(".jsonl") || s.ends_with(".jsonl.zst") {
                top_jsonl = true;
            }
        } else if ft.is_dir()
            && let Ok(rd) = fs::read_dir(e.path())
        {
            for inner in rd.flatten() {
                if inner.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    let s = inner.file_name().to_string_lossy().into_owned();
                    if s.ends_with(".jsonl") || s.ends_with(".jsonl.zst") {
                        subdir_jsonl = true;
                        break;
                    }
                }
            }
            if subdir_jsonl {
                break;
            }
        }
    }
    Ok(if top_jsonl {
        Layout::Flat
    } else if subdir_jsonl {
        Layout::Projected
    } else {
        Layout::Empty
    })
}

/// Move/compress one session bundle (jsonl + sibling dir + sidecar).
/// `src_jsonl` may be either `<x>.jsonl` (raw) or `<x>.jsonl.zst`.
/// `dst_zst` is the target `<x>.jsonl.zst` in the store layout.
/// If a sibling dir `<x>/` exists, it is moved to the same parent as
/// `dst_zst` with the same stem.
fn migrate_one_bundle(src_jsonl_or_zst: &Path, dst_zst: &Path, dry_run: bool) -> io::Result<u64> {
    let src_name = src_jsonl_or_zst
        .file_name()
        .ok_or_else(|| io::Error::other("no file name"))?
        .to_string_lossy()
        .into_owned();
    let parent_src = src_jsonl_or_zst
        .parent()
        .ok_or_else(|| io::Error::other("no parent"))?;
    let parent_dst = dst_zst
        .parent()
        .ok_or_else(|| io::Error::other("no parent"))?;

    // Determine stem (without .jsonl or .jsonl.zst).
    let stem = if let Some(s) = src_name.strip_suffix(".jsonl.zst") {
        s.to_string()
    } else if let Some(s) = src_name.strip_suffix(".jsonl") {
        s.to_string()
    } else {
        return Err(io::Error::other(format!(
            "not a session file: {}",
            src_jsonl_or_zst.display()
        )));
    };
    let size = src_jsonl_or_zst.metadata()?.len();

    if dry_run {
        let kind = if src_name.ends_with(".jsonl.zst") {
            "move"
        } else {
            "compress+move"
        };
        println!("would {kind} {} -> {}", src_jsonl_or_zst.display(), dst_zst.display());
        let sib_src = parent_src.join(&stem);
        if sib_src.is_dir() {
            let sib_dst = parent_dst.join(&stem);
            println!("would mv-dir {} -> {}", sib_src.display(), sib_dst.display());
        }
        return Ok(size);
    }

    fs::create_dir_all(parent_dst)?;

    // 1) The session file.
    if src_name.ends_with(".jsonl.zst") {
        // Already compressed; just rename. Same with sidecar if present.
        if dst_zst != src_jsonl_or_zst {
            fs::rename(src_jsonl_or_zst, dst_zst)?;
            let src_meta = sidecar_path(src_jsonl_or_zst);
            let dst_meta = sidecar_path(dst_zst);
            let _ = fs::rename(&src_meta, &dst_meta);
        }
    } else {
        atomic_compress_to_store(src_jsonl_or_zst, dst_zst)?;
        fs::remove_file(src_jsonl_or_zst)?;
    }

    // 2) The sibling bundle dir, if any.
    let sib_src = parent_src.join(&stem);
    let sib_dst = parent_dst.join(&stem);
    if sib_src.is_dir() && sib_src != sib_dst {
        fs::rename(&sib_src, &sib_dst)?;
    }

    Ok(size)
}

fn migrate_dir(
    from: &Path,
    into_subdir: Option<&str>,
    store: &Path,
    dry_run: bool,
) -> io::Result<(usize, u64)> {
    let layout = detect_layout(from)?;
    let mut count = 0usize;
    let mut bytes = 0u64;
    match layout {
        Layout::Empty => {
            println!("source is empty; nothing to migrate");
        }
        Layout::Flat => {
            let proj = into_subdir.ok_or_else(|| {
                io::Error::other("flat source needs --into-subdir / symlink name")
            })?;
            let dst_dir = store.join(proj);
            // Snapshot entries up-front because we'll be moving things.
            let entries: Vec<_> = fs::read_dir(from)?.flatten().collect();
            for e in entries {
                let ft = match e.file_type() {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                if !ft.is_file() {
                    continue;
                }
                let name = e.file_name().to_string_lossy().into_owned();
                let stem = if let Some(s) = name.strip_suffix(".jsonl.zst") {
                    s.to_string()
                } else if let Some(s) = name.strip_suffix(".jsonl") {
                    s.to_string()
                } else {
                    continue;
                };
                let dst_zst = dst_dir.join(format!("{stem}.{JSONL_EXT}.{ZST_EXT}"));
                let size = migrate_one_bundle(&e.path(), &dst_zst, dry_run)?;
                count += 1;
                bytes += size;
            }
        }
        Layout::Projected => {
            for proj in fs::read_dir(from)?.flatten() {
                if !proj.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let proj_name = proj.file_name();
                let dst_dir = store.join(&proj_name);
                let inner_entries: Vec<_> = match fs::read_dir(proj.path()) {
                    Ok(r) => r.flatten().collect(),
                    Err(_) => continue,
                };
                for e in inner_entries {
                    let ft = match e.file_type() {
                        Ok(t) => t,
                        Err(_) => continue,
                    };
                    if !ft.is_file() {
                        continue;
                    }
                    let name = e.file_name().to_string_lossy().into_owned();
                    let stem = if let Some(s) = name.strip_suffix(".jsonl.zst") {
                        s.to_string()
                    } else if let Some(s) = name.strip_suffix(".jsonl") {
                        s.to_string()
                    } else {
                        continue;
                    };
                    let dst_zst = dst_dir.join(format!("{stem}.{JSONL_EXT}.{ZST_EXT}"));
                    let size = migrate_one_bundle(&e.path(), &dst_zst, dry_run)?;
                    count += 1;
                    bytes += size;
                }
            }
        }
    }
    Ok((count, bytes))
}

fn cmd_migrate_store(from: &str, dry_run: bool) -> io::Result<()> {
    let from_path = expand_tilde(from);
    let (canonical, into_subdir) = if from_path.is_symlink() {
        let target = fs::canonicalize(&from_path)?;
        let name = from_path
            .file_name()
            .and_then(|n| n.to_str())
            .map(String::from);
        (target, name)
    } else {
        (from_path.clone(), None)
    };
    if !canonical.is_dir() {
        return Err(io::Error::other(format!("not a directory: {}", from_path.display())));
    }
    let store = store_dir()?;
    fs::create_dir_all(&store)?;
    let (count, bytes) = migrate_dir(&canonical, into_subdir.as_deref(), &store, dry_run)?;
    println!(
        "{} {count} bundles ({})",
        if dry_run { "would migrate" } else { "migrated" },
        fmt_size(bytes)
    );
    Ok(())
}

// ── Install ─────────────────────────────────────────────────────────────────

const SYSTEMD_UNIT: &str = include_str!("../systemd/claude-cellar.service");

struct MigrationPlan {
    from: PathBuf,
    into_subdir: Option<String>,
    remove_symlink: Option<PathBuf>,
}

fn plan_install(mount_dir_path: &Path) -> io::Result<(PathBuf, Option<MigrationPlan>)> {
    let default_store = store_dir()?;
    if !mount_dir_path.exists() || !mount_dir_path.is_dir() {
        return Ok((default_store, None));
    }
    let entries: Vec<_> = fs::read_dir(mount_dir_path)?.flatten().collect();
    if entries.is_empty() {
        return Ok((default_store, None));
    }
    let mut symlinks: Vec<_> = entries.iter().filter(|e| e.path().is_symlink()).collect();
    if symlinks.len() == 1 {
        let sym = symlinks.remove(0);
        let target = fs::canonicalize(sym.path())?;
        let name = sym
            .file_name()
            .to_str()
            .ok_or_else(|| io::Error::other("symlink name not utf-8"))?
            .to_string();
        let other = entries.len() - 1;
        if other > 0 {
            println!(
                "note: {other} other entries in {} are not migrated and will be hidden by the FUSE mount; uninstall to access them.",
                mount_dir_path.display()
            );
        }
        return Ok((
            target.clone(),
            Some(MigrationPlan {
                from: target,
                into_subdir: Some(name.clone()),
                remove_symlink: Some(sym.path()),
            }),
        ));
    }
    Ok((
        default_store.clone(),
        Some(MigrationPlan {
            from: mount_dir_path.to_path_buf(),
            into_subdir: None,
            remove_symlink: None,
        }),
    ))
}

fn execute_plan(plan: &MigrationPlan, store: &Path) -> io::Result<()> {
    println!("migrating bundles from {} → {}", plan.from.display(), store.display());
    let (n, bytes) = migrate_dir(&plan.from, plan.into_subdir.as_deref(), store, false)?;
    println!("migrated {n} bundles ({})", fmt_size(bytes));
    if let Some(sym) = &plan.remove_symlink {
        fs::remove_file(sym)?;
        println!("removed symlink {} (FUSE replaces it)", sym.display());
    }
    Ok(())
}

fn cmd_install(no_systemd: bool, claude_bin: Option<PathBuf>) -> io::Result<()> {
    if let Some(p) = claude_bin {
        if !p.exists() {
            return Err(io::Error::other(format!(
                "--claude-bin not found: {}",
                p.display()
            )));
        }
        let canon = fs::canonicalize(&p)?;
        write_stored_claude_bin(&canon)?;
        println!("claude binary stored: {}", canon.display());
    } else if let Ok(p) = resolve_claude_bin() {
        write_stored_claude_bin(&p)?;
        println!("claude binary auto-detected: {}", p.display());
    }

    let mp = mount_dir()?;
    let (store, migration) = plan_install(&mp)?;
    let default_store = store_dir()?;
    let store_is_default = store == default_store;

    println!("store: {}", store.display());
    println!(
        "store layout: {}",
        if store_is_default { "default" } else { "custom" }
    );

    if let Some(plan) = &migration {
        execute_plan(plan, &store)?;
    } else {
        println!("no existing sessions to migrate");
    }

    if no_systemd {
        println!("--no-systemd: skipping systemd registration. Manual: claude-cellar mount");
        return Ok(());
    }

    let unit_dir = dirs::config_dir()
        .ok_or_else(|| io::Error::other("no XDG_CONFIG_HOME"))?
        .join("systemd")
        .join("user");
    fs::create_dir_all(&unit_dir)?;
    let unit_path = unit_dir.join("claude-cellar.service");
    let self_exe = std::env::current_exe()?;
    let mut unit = SYSTEMD_UNIT.replace("{{CELLAR_BIN}}", &self_exe.display().to_string());
    if !store_is_default {
        let line = format!("Environment=CLAUDE_CELLAR_STORE_DIR={}", store.display());
        unit = unit.replacen("[Service]\n", &format!("[Service]\n{line}\n"), 1);
    }
    fs::write(&unit_path, unit)?;
    println!("systemd unit installed: {}", unit_path.display());

    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    let st = Command::new("systemctl")
        .args(["--user", "enable", "--now", "claude-cellar.service"])
        .status()?;
    if !st.success() {
        return Err(io::Error::other("systemctl enable --now failed"));
    }

    let mut mounted = false;
    for _ in 0..30 {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if is_path_mounted_with_cellar(&mp) {
            mounted = true;
            break;
        }
    }
    if !mounted {
        eprintln!(
            "WARNING: service started but mount at {} not yet visible. Check `claude-cellar status` and `journalctl --user -u claude-cellar.service`.",
            mp.display()
        );
    } else {
        println!("\nclaude-cellar.service enabled and started.");
        println!("Mount active at {}.", mp.display());
        println!("\nUse `claude` as always. Run `claude-cellar status` for state.");
    }
    log_info(&format!("install store={} migration={}", store.display(), migration.is_some()));
    Ok(())
}

fn cmd_uninstall() -> io::Result<()> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "claude-cellar.service"])
        .status();
    let unit_path = dirs::config_dir()
        .ok_or_else(|| io::Error::other("no XDG_CONFIG_HOME"))?
        .join("systemd")
        .join("user")
        .join("claude-cellar.service");
    if unit_path.is_file() {
        fs::remove_file(&unit_path)?;
        println!("removed: {}", unit_path.display());
    }
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).status();
    println!("\nclaude-cellar uninstalled. Store at {} is preserved.", store_dir()?.display());
    Ok(())
}

// ── Claude binary discovery ─────────────────────────────────────────────────

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
    let _ = config_dir()?;
    let p = claude_bin_config_file()?;
    fs::write(&p, format!("{}\n", path.display()))
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
            if let Some(latest) = vs.into_iter().last() {
                out.push(latest);
            }
        }
    }
    out
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
        return Ok(canon);
    }
    Err(io::Error::other("claude binary not found"))
}

fn default_projects_dir() -> io::Result<PathBuf> {
    if let Ok(custom) = std::env::var("CLAUDE_CELLAR_PROJECTS_DIR")
        && !custom.is_empty()
    {
        return Ok(PathBuf::from(custom));
    }
    let home = dirs::home_dir().ok_or_else(|| io::Error::other("no home"))?;
    Ok(home.join(".claude").join("projects"))
}

// ── Log ─────────────────────────────────────────────────────────────────────

fn cmd_log(tail: usize) -> io::Result<()> {
    let Some(path) = log_path() else {
        eprintln!("no log path");
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

// ── Dispatch ────────────────────────────────────────────────────────────────

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
            let _ = append_ext;
            0
        }
        Subcmd::Decompress { file, output } => {
            let dst = decompress_file(&file, output.as_deref())?;
            println!("decompressed -> {}", dst.display());
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
        Subcmd::Mount {
            foreground,
            store_dir,
            mount_dir,
        } => {
            cmd_mount(foreground, store_dir, mount_dir)?;
            0
        }
        Subcmd::Umount { mount_dir } => {
            cmd_umount(mount_dir)?;
            0
        }
        Subcmd::Status => {
            cmd_status()?;
            0
        }
        Subcmd::MigrateStore { from, dry_run } => {
            cmd_migrate_store(&from, dry_run)?;
            0
        }
        Subcmd::Install {
            no_systemd,
            claude_bin,
        } => {
            cmd_install(no_systemd, claude_bin)?;
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
