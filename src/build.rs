//! Turning a revision into the project's binary, once per source tree.
//!
//! Every revision becomes a git tree id: a commit's tree, or for the working tree a
//! snapshot written through a throwaway index (the real index and the files are not
//! touched; untracked files that .gitignore lets through are included). The binary is
//! kept under `<data>/bin/<tree>.<tag>/<bin>` (`tag` in gate.conf), so the same sources never build twice.
//!
//! Building happens in one persistent worktree, `<data>/wt`, with its own target dir.
//! A checkout rewrites only the files that differ, so their mtimes move and cargo
//! rebuilds just those crates. Its own target, not the main one: two trees sharing a
//! target can hand back the other tree's artifacts as fresh.

use crate::exec;
use crate::history::{self, Rec};
use crate::{Ctx, md5_file, now, sh_out};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

/// What the build runs with besides gate.conf's `env`: the caller's settings would make another binary of the
/// same tree, and CPU compared across build settings means nothing.
const DROPPED_ENV: [&str; 2] = ["CARGO_INCREMENTAL", "RUSTFLAGS"];

pub struct Bin {
    pub path: PathBuf,
    pub md5: String,
    /// The tree id, or the path given with --bin.
    pub rev: String,
}

pub fn resolve(ctx: &Ctx, rev: Option<&str>) -> Result<Bin, String> {
    // Absolute: cases run from their own directory.
    if let Some(p) = rev.and_then(|r| std::fs::canonicalize(r).ok()).filter(|p| p.is_file()) {
        let md5 = md5_file(&p)?;
        let rev = p.to_string_lossy().into_owned();
        return Ok(Bin { path: p, md5, rev });
    }
    let git = |args: &[&str]| sh_out(Command::new("git").arg("-C").arg(&ctx.root).args(args));
    let (tree, commit) = match rev {
        None | Some("tree") => (snapshot(ctx)?, None),
        Some(r) => {
            let commit = git(&["rev-parse", "--verify", &format!("{r}^{{commit}}")])?;
            (git(&["rev-parse", &format!("{commit}^{{tree}}")])?, Some(commit))
        }
    };
    if ctx.project.build.is_empty() {
        return Err(format!("{} names no build (keys build and bin), so there is nothing to build; --bin PATH runs a binary as it is", crate::project::FILE));
    }
    // `<tree>.<tag>`: history compares CPU only between binaries of the same tag.
    let key = format!("{tree}.{}", ctx.project.tag());
    let bin = ctx.data.join("bin").join(&key).join(ctx.project.bin_name());
    // mtime is last use: gc drops the binaries used longest ago.
    if let Ok(f) = File::options().write(true).open(&bin) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
    if !bin.is_file() {
        let _lock = lock(&ctx.data.join("build.lock"))?;
        // Another gate may have built it while we waited for the lock.
        if !bin.is_file() {
            let commit = match commit {
                Some(c) => c,
                None => git(&["commit-tree", &tree, "-p", "HEAD", "-m", "regate: working tree snapshot"])?,
            };
            build(ctx, &key, &commit, &bin)?;
            gc(ctx);
        }
    }
    Ok(Bin { md5: md5_file(&bin)?, path: bin, rev: key })
}

fn snapshot(ctx: &Ctx) -> Result<String, String> {
    // `tree` is the checkout the command was called from, a worktree too: the root holds another session's work.
    let dir = match crate::env("ROOT") {
        Some(_) => ctx.root.clone(),
        None => PathBuf::from(sh_out(Command::new("git").args(["rev-parse", "--show-toplevel"]))?),
    };
    let git_dir = sh_out(Command::new("git").arg("-C").arg(&dir).args(["rev-parse", "--absolute-git-dir"]))?;
    let index = ctx.data.join(format!("snapshot-{}.index", std::process::id()));
    // Starting from the real index makes `add -A` hash only what changed.
    let _ = std::fs::copy(PathBuf::from(git_dir).join("index"), &index);
    let git = |args: &[&str]| {
        sh_out(Command::new("git").arg("-C").arg(&dir).env("GIT_INDEX_FILE", &index).args(args))
    };
    let tree = git(&["add", "-A"]).and_then(|_| git(&["write-tree"]));
    let _ = std::fs::remove_file(&index);
    tree
}

fn build(ctx: &Ctx, tree: &str, commit: &str, bin: &PathBuf) -> Result<(), String> {
    let wt = ctx.data.join("wt");
    let git = |c: &mut Command| sh_out(c).map(|_| ());
    if wt.join(".git").exists() {
        git(Command::new("git").arg("-C").arg(&wt).args(["checkout", "--quiet", "--force", "--detach", commit]))?;
    } else {
        git(Command::new("git").arg("-C").arg(&ctx.root).args(["worktree", "add", "--quiet", "--detach"]).arg(&wt).arg(commit))?;
    }
    let logs = ctx.data.join("logs");
    std::fs::create_dir_all(&logs).map_err(|e| e.to_string())?;
    let log_path = logs.join(format!("build-{tree}.log"));
    let log = File::create(&log_path).map_err(|e| e.to_string())?;
    let target = ctx.data.join("target");
    eprintln!("building {} ({}) -> {}", &tree[..10], &commit[..10], log_path.display());
    let ts = now();
    let load = crate::loadavg();
    // The machine-wide build queue every session builds through; it sets the threads (CARGO_BUILD_JOBS).
    let mut cmd = Command::new("/proc/self/exe");
    cmd.arg("queue")
        .arg("--config")
        .arg(ctx.data.join("limits.conf"))
        .args(["--kind", "build", "--"])
        .args(&ctx.project.build)
        .current_dir(&wt)
        .env("CARGO_TARGET_DIR", &target)
        .stdin(Stdio::null())
        .stdout(log.try_clone().map_err(|e| e.to_string())?)
        .stderr(log);
    for k in DROPPED_ENV {
        cmd.env_remove(k);
    }
    cmd.envs(ctx.project.env.iter().map(|(k, v)| (k, v)));
    // Timed from the exec into cargo: the queue is not the build's time.
    let o = exec::run(&mut cmd, Some(&log_path.with_extension("started")), Duration::from_secs(4 * 3600)).map_err(|e| format!("build: {e}"))?;
    let built = wt.join(&ctx.project.bin);
    let md5 = if o.status == "ok" { md5_file(&built)? } else { String::new() };
    let rec = Rec {
        ts,
        kind: "build".into(),
        // The build's last word: its target, for a cargo build.
        name: ctx.project.build.last().unwrap().clone(),
        bin: md5,
        rev: tree.into(),
        status: o.status.clone(),
        wall_s: o.wall_s,
        cpu_s: o.cpu_s,
        rss_mb: o.rss_mb,
        wait_s: o.wait_s,
        load,
        log: log_path.to_string_lossy().into_owned(),
        ..Default::default()
    };
    history::append(&ctx.history(), &rec).map_err(|e| e.to_string())?;
    if o.status != "ok" {
        return Err(format!("build {}: {}, see {}", &tree[..10], o.status, log_path.display()));
    }
    eprintln!("built in {:.0} s (cpu {:.0} s, peak {:.1} GB, queue {:.0} s)", o.wall_s, o.cpu_s, o.rss_mb / 1024.0, o.wait_s);
    let dir = bin.parent().unwrap();
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // Copied, not linked: the next build overwrites the target's binary.
    let tmp = bin.with_extension("tmp");
    std::fs::copy(&built, &tmp).and_then(|_| std::fs::rename(&tmp, bin)).map_err(|e| e.to_string())
}

/// Keeps the newest `[gate] keep_binaries` binaries (by last use) with their logs and run
/// directories; history stays whole. Nothing to do without the key.
fn gc(ctx: &Ctx) {
    let keep = std::fs::read_to_string(ctx.data.join("limits.conf")).ok().and_then(|t| crate::queue::Limits::parse(&t).ok()?.keep_binaries);
    let Some(keep) = keep else { return };
    let mut bins: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(ctx.data.join("bin"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| Some((e.path().join(ctx.project.bin_name()).metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    bins.sort_by_key(|b| std::cmp::Reverse(b.0));
    for (_, dir) in bins.into_iter().skip(keep.max(1)) {
        let tree = dir.file_name().unwrap().to_string_lossy().into_owned();
        if let Ok(md5) = md5_file(&dir.join(ctx.project.bin_name())) {
            let _ = std::fs::remove_dir_all(ctx.data.join("logs").join(&md5));
            let _ = std::fs::remove_dir_all(ctx.data.join("out").join(&md5));
        }
        let _ = std::fs::remove_file(ctx.data.join("logs").join(format!("build-{tree}.log")));
        let _ = std::fs::remove_dir_all(&dir);
        eprintln!("gc: dropped binary {}", &tree[..10.min(tree.len())]);
    }
}

/// An exclusive flock held for the life of the returned file.
fn lock(path: &PathBuf) -> Result<File, String> {
    let f = File::create(path).map_err(|e| e.to_string())?;
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        eprintln!("another regate is building; waiting");
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!("flock {}: {}", path.display(), std::io::Error::last_os_error()));
        }
    }
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gc_keeps_the_newest_used_binaries_and_drops_their_logs() {
        let data = std::env::temp_dir().join(format!("gate-gc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&data);
        let ctx = Ctx { root: data.clone(), data: data.clone(), project: Default::default(), remote: None };
        let t0 = std::time::SystemTime::now() - Duration::from_secs(1000);
        for (i, tree) in ["aaa", "bbb", "ccc"].iter().enumerate() {
            std::fs::create_dir_all(data.join("bin").join(tree)).unwrap();
            let bin = data.join("bin").join(tree).join("bin");
            std::fs::write(&bin, tree).unwrap();
            File::options().write(true).open(&bin).unwrap().set_modified(t0 + Duration::from_secs(i as u64 * 10)).unwrap();
            let md5 = md5_file(&bin).unwrap();
            std::fs::create_dir_all(data.join("logs").join(&md5)).unwrap();
            std::fs::write(data.join("logs").join(format!("build-{tree}.log")), "").unwrap();
        }
        gc(&ctx); // no limits.conf: keeps everything
        assert_eq!(std::fs::read_dir(data.join("bin")).unwrap().count(), 3);
        std::fs::write(data.join("limits.conf"), "[gate]\nkeep_binaries = 2\n").unwrap();
        gc(&ctx);
        assert!(!data.join("bin/aaa").exists() && data.join("bin/bbb").exists() && data.join("bin/ccc").exists());
        assert!(!data.join("logs/build-aaa.log").exists() && data.join("logs/build-bbb.log").exists());
        assert_eq!(std::fs::read_dir(data.join("logs")).unwrap().filter(|e| e.as_ref().unwrap().path().is_dir()).count(), 2);
        std::fs::remove_dir_all(data).unwrap();
    }
}
