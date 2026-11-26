//! `cargo test`, `clippy` and `check` of a session's tree on the machine <data>/remote.conf names: only
//! their output matters here, and this box's queue waits on memory while that one idles. The tree goes there
//! by rsync (its own directory per tree, mtimes kept, so cargo's build there stays incremental) with the
//! files under data/ its tests name; all trees share one target there. The job waits in that machine's
//! queue and its output streams here. `cargo build`/`run`/`install` stay here: their binary is used here.
//! REGATE_BUILD_LOCAL=1 runs it here.

use crate::queue::Job;
use crate::remote::Remote;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The machine there keeps this much free; a build that would leave less runs nowhere until it is cleaned.
const MIN_FREE_MB: u64 = 30_000;
/// A path under data/ larger than this is not sent for tests (a whole data/test is hundreds of GB).
const DATA_MAX_MB: u64 = 6_000;
/// What a wrapper script or a session sets that changes what cargo builds or how tests run.
const FORWARD: [&str; 7] = ["RUSTFLAGS", "RUSTC_BOOTSTRAP", "CARGO_INCREMENTAL", "RUST_BACKTRACE", "RUST_TEST_THREADS", "RUST_LOG", "CARGO_BUILD_JOBS"];

/// The cargo subcommand of `cmd`, if it is one whose output is all there is.
fn output_only(cmd: &[String]) -> Option<&str> {
    let (prog, args) = cmd.split_first()?;
    (prog.rsplit('/').next() == Some("cargo")).then_some(())?;
    let sub = args.iter().find(|a| !a.starts_with('-') && !a.starts_with('+'))?;
    matches!(sub.as_str(), "test" | "clippy" | "check").then_some(sub.as_str())
}

/// Runs the job there when it can; `None`: run it here.
pub fn run(job: &Job) -> Result<Option<i32>, String> {
    if crate::env("BUILD_LOCAL").is_some() || output_only(&job.cmd).is_none() {
        return Ok(None);
    }
    let Some(gate_dir) = job.config.parent() else { return Ok(None) };
    let Some(r) = Remote::load(gate_dir)? else { return Ok(None) };
    let Some(root) = gate_dir.parent().and_then(Path::parent) else { return Ok(None) };
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let Ok(top) = crate::sh_out(Command::new("git").args(["rev-parse", "--show-toplevel"])) else { return Ok(None) };
    let tree = PathBuf::from(top);
    // A tree outside the project, or an argument naming a file outside the tree: here.
    let Ok(rel) = tree.strip_prefix(root) else { return Ok(None) };
    if job.cmd.iter().any(|a| (a.starts_with('/') && !Path::new(a).starts_with(&tree)) || a.starts_with('~')) {
        return Ok(None);
    }
    let key = if rel.as_os_str().is_empty() { "root".to_string() } else { rel.to_string_lossy().replace('/', "_") };
    let rtree = format!("{}/trees/{key}", r.root);
    let free = r.sh(&format!("mkdir -p {t} && df --output=avail -m {t} | tail -1", t = quote(&rtree)))?;
    let free: u64 = free.trim().parse().map_err(|_| format!("{}: df said {free}", r.dest))?;
    if free < MIN_FREE_MB {
        return Err(format!(
            "{} has {free} MB free, under {MIN_FREE_MB}: clean its target ({}/target) or run here with REGATE_BUILD_LOCAL=1",
            r.dest, r.root
        ));
    }
    eprintln!("build: {} on {} ({}/remote.conf; REGATE_BUILD_LOCAL=1 runs it here)", output_only(&job.cmd).unwrap_or(""), r.dest, gate_dir.display());
    r.sync_gate(root)?;
    crate::sh_out(
        r.rsync()
            .args(["--delete", "--exclude=/data", "--exclude=/.git", "--exclude=/target*", "--exclude=/.venv*", "--exclude=/.claude", "--exclude=/.regate", "--exclude=node_modules/"])
            .arg(format!("{}/", tree.display()))
            .arg(format!("{}:{rtree}/", r.dest)),
    )
    .map_err(|e| format!("sending {}: {e}", tree.display()))?;
    // The tree's data/ is the project's (the root's own, or a worktree's link to it): the same there.
    if tree.join("data").exists() {
        let data = test_data(&tree, root);
        if !data.is_empty() {
            let mut c = r.rsync();
            c.arg("-R");
            for p in &data {
                c.arg(root.join(".").join(p));
            }
            crate::sh_out(c.arg(format!("{}:{}/", r.dest, r.root))).map_err(|e| format!("sending test data: {e}"))?;
        }
        r.sh(&format!("[ -e {t}/data ] || ln -s {d} {t}/data", t = quote(&rtree), d = quote(&format!("{}/data", r.root))))?;
    }
    let sub = cwd.strip_prefix(&tree).unwrap_or(Path::new(""));
    let user = job.user.clone().unwrap_or_else(crate::queue::default_user);
    let mut envs = format!("PATH=\"$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin\" CARGO_TARGET_DIR={} REGATE_QUEUE_USER={} ", quote(&format!("{}/target", r.root)), quote(&user));
    for (k, v) in std::env::vars().filter(|(k, _)| FORWARD.contains(&k.as_str()) || k.starts_with("CARGO_PROFILE_")) {
        envs += &format!("{k}={} ", quote(&v));
    }
    let mut opts = String::new();
    if let Some(mb) = job.mem_mb {
        opts += &format!(" -m {mb}M");
    }
    if let Some(t) = job.threads {
        opts += &format!(" -t {t}");
    }
    let script = format!(
        "cd {d} && exec env {envs}{gate} queue --config {limits} --kind build{opts} -- {cmd}",
        d = quote(&format!("{rtree}/{}", sub.to_string_lossy())),
        gate = quote(&format!("{}/regate", r.data)),
        limits = quote(&format!("{}/limits.conf", r.data)),
        cmd = job.cmd.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "),
    );
    let status = r.ssh().arg(script).status().map_err(|e| format!("ssh: {e}"))?;
    match status.code() {
        Some(255) => Err(format!("ssh to {} failed; REGATE_BUILD_LOCAL=1 runs it here", r.dest)),
        Some(c) => Ok(Some(c)),
        None => Ok(Some(1)),
    }
}

/// The paths under data/ the tree's Rust sources name (`../../data/test/carve`, `"data/rules"`), those
/// that exist and are not too large to send; the gate's data and Python venvs (absolute paths inside) are not.
fn test_data(tree: &Path, root: &Path) -> Vec<PathBuf> {
    let Ok(found) = crate::sh_out(Command::new("git").arg("-C").arg(tree).args(["grep", "-ohE", r"(\.\./)*data/[A-Za-z0-9_./-]+", "--", "*.rs"])) else {
        return vec![];
    };
    let mut paths: Vec<PathBuf> = found
        .lines()
        .map(|l| l.trim_start_matches("../").trim_end_matches(['/', '.']))
        .filter(|l| l.matches('/').count() >= 1 && !l.starts_with("data/gate") && !l.starts_with("data/regate") && !l.starts_with(".regate") && !l.contains("venv"))
        .map(PathBuf::from)
        .collect();
    paths.sort();
    paths.dedup();
    // A path under another one listed goes with it.
    let tops: Vec<PathBuf> = paths.iter().filter(|p| !paths.iter().any(|q| q != *p && p.starts_with(q))).cloned().collect();
    tops.into_iter().filter(|p| root.join(p).exists() && size_mb(&root.join(p)) <= DATA_MAX_MB).collect()
}

fn size_mb(p: &Path) -> u64 {
    crate::sh_out(Command::new("du").arg("-sm").arg(p)).ok().and_then(|o| o.split_whitespace().next()?.parse().ok()).unwrap_or(u64::MAX)
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_what_prints_goes_there() {
        let v = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(output_only(&v("cargo test --release -p core")), Some("test"));
        assert_eq!(output_only(&v("cargo +1.98 clippy -p x -- -D warnings")), Some("clippy"));
        assert_eq!(output_only(&v("/home/u/.cargo/bin/cargo check")), Some("check"));
        assert_eq!(output_only(&v("cargo build --release -p app")), None);
        assert_eq!(output_only(&v("cargo install --path .")), None);
        assert_eq!(output_only(&v("env X=1 cargo test")), None);
    }
}
