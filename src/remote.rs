//! Cases on another machine: `<data>/remote.conf` names it, the binary and the inputs go there
//! by rsync, the case runs there under that machine's own queue and is measured there, and its
//! output and log come back here, so hashing, history and diff stay on this machine.
//!
//! remote.conf:
//!   ssh = <ssh options> user@host
//!   root = /absolute/project/root/on/that/machine

use crate::exec::Outcome;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

pub struct Remote {
    /// ssh options, without the destination.
    opts: Vec<String>,
    pub dest: String,
    pub root: String,
    /// The data dir there: the same name as here under its root.
    pub data: String,
}

/// A dead link ends the case instead of hanging it: no answer for a minute closes the connection.
const KEEPALIVE: [&str; 6] = ["-o", "BatchMode=yes", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=4"];

/// Binaries no run has used for two days are removed there: 86 MB each, about 200 builds a week.
const BIN_KEEP_MIN: u32 = 2 * 24 * 60;

impl Remote {
    pub fn load(data: &Path) -> Result<Option<Remote>, String> {
        let path = data.join("remote.conf");
        let Ok(text) = std::fs::read_to_string(&path) else { return Ok(None) };
        let mut r = Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        r.data = format!("{}/data/{}", r.root, data.file_name().unwrap_or_default().to_string_lossy());
        Ok(Some(r))
    }

    fn parse(text: &str) -> Result<Remote, String> {
        let (mut ssh, mut root) = (None, None);
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (k, v) = line.split_once('=').ok_or(format!("not key = value: {line}"))?;
            match k.trim() {
                "ssh" => ssh = Some(v.split_whitespace().map(String::from).collect::<Vec<_>>()),
                "root" => root = Some(v.trim().trim_end_matches('/').to_string()),
                k => return Err(format!("unknown key {k}")),
            }
        }
        let mut opts = ssh.ok_or("no ssh =")?;
        let dest = opts.pop().ok_or("ssh = names no destination")?;
        let root = root.ok_or("no root =")?;
        if !root.starts_with('/') {
            return Err("root must be absolute".into());
        }
        let data = format!("{root}/data/regate");
        Ok(Remote { opts, dest, root, data })
    }

    pub fn ssh(&self) -> Command {
        let mut c = Command::new("ssh");
        c.args(KEEPALIVE).args(&self.opts).arg(&self.dest).stdin(Stdio::null());
        c
    }

    /// One shell command there; its stdout.
    pub fn sh(&self, script: &str) -> Result<String, String> {
        crate::sh_out(self.ssh().arg(script)).map_err(|e| format!("{}: {e}", self.dest))
    }

    pub fn rsync(&self) -> Command {
        let mut e = vec!["ssh".to_string()];
        e.extend(KEEPALIVE.iter().map(|s| s.to_string()));
        e.extend(self.opts.iter().cloned());
        let mut c = Command::new("rsync");
        c.arg("-a").arg("-e").arg(e.join(" ")).stdin(Stdio::null());
        c
    }

    pub fn path(&self, root: &Path, local: &Path) -> Result<String, String> {
        let rel = local.strip_prefix(root).map_err(|_| format!("{} is outside {}", local.display(), root.display()))?;
        Ok(format!("{}/{}", self.root, rel.to_string_lossy()))
    }

    /// Where the binary `local` is kept there, under its own name.
    pub fn bin(&self, md5: &str, local: &Path) -> String {
        format!("{}/bin/{md5}/{}", self.data, local.file_name().unwrap_or_default().to_string_lossy())
    }

    /// The gate itself, the queue's limits (kept as that machine has them once there), the binary and
    /// every input, each sent only when it differs.
    pub fn prepare(&self, root: &Path, bin_path: &Path, bin_md5: &str, inputs: &[PathBuf]) -> Result<(), String> {
        let (r, d) = (&self.root, &self.data);
        let bin = self.bin(bin_md5, bin_path);
        let present = self.sh(&format!(
            "mkdir -p {d}/bin {d}/out {d}/logs && \
             find {d}/bin -mindepth 1 -maxdepth 1 -mmin +{BIN_KEEP_MIN} -exec rm -rf {{}} + ; \
             test -x {b} && touch {b} && echo yes || true",
            b = quote(&bin)
        ))?;
        self.sync_gate(root)?;
        if present != "yes" {
            self.sh(&format!("mkdir -p {}", quote(&format!("{d}/bin/{bin_md5}"))))?;
            crate::sh_out(self.rsync().arg(bin_path).arg(format!("{}:{bin}", self.dest)))?;
            self.sh(&format!("touch {}", quote(&bin)))?;
        }
        if !inputs.is_empty() {
            // `root/./rel` keeps rel under the root there (-R).
            let mut c = self.rsync();
            c.arg("-R");
            for p in inputs {
                let rel = p.strip_prefix(root).map_err(|_| format!("{} is outside {}", p.display(), root.display()))?;
                c.arg(root.join(".").join(rel));
            }
            crate::sh_out(c.arg(format!("{}:{r}/", self.dest)))?;
        }
        Ok(())
    }

    /// This gate itself and the queue's limits (kept as that machine has them once there).
    pub fn sync_gate(&self, root: &Path) -> Result<(), String> {
        let d = &self.data;
        self.sh(&format!("mkdir -p {d}"))?;
        // This process's own image, not the path it was started from: a `cargo install` during a run replaces
        // that path, readlink then says "... (deleted)" and every case failed with "gate error: remote".
        let gate = format!("/proc/{}/exe", std::process::id());
        crate::sh_out(self.rsync().arg("-L").arg(&gate).arg(format!("{}:{d}/regate", self.dest)))?;
        let limits = crate::data_dir(root).join("limits.conf");
        crate::sh_out(self.rsync().arg("--ignore-existing").arg(&limits).arg(format!("{}:{d}/limits.conf", self.dest)))?;
        Ok(())
    }

    /// Runs `cmd` (already in that machine's paths) in the binary's output directory there, measured
    /// there; then brings `name` back into `local_out`, its measures to `measures`, and appends the run's log
    /// to `log`.
    #[allow(clippy::too_many_arguments)]
    pub fn run(&self, md5: &str, name: &str, env: &[(&str, String)], measures_var: Option<&str>, cmd: &[String], timeout: Duration, local_out: &Path, log: &Path, measures: &Path) -> Result<Outcome, String> {
        let d = &self.data;
        let out_dir = format!("{d}/out/{md5}");
        let log_dir = format!("{d}/logs/{md5}");
        let rlog = format!("{log_dir}/{name}.log");
        let rmeasures = format!("{log_dir}/{name}.measures.jsonl");
        // gate.conf's `measures` names it to the program too.
        let extra: Vec<(&str, String)> = std::iter::once("REGATE_MEASURES").chain(measures_var).map(|k| (k, rmeasures.clone())).collect();
        let envs: String = env.iter().chain(&extra).map(|(k, v)| format!("{k}={} ", quote(v))).collect();
        let script = format!(
            "mkdir -p {od} {ld} && cd {od} && rm -rf {n} {rm} && exec env {envs}{gate} measure --timeout {t} --log {l} --marker {m} -- {c}",
            od = quote(&out_dir),
            ld = quote(&log_dir),
            n = quote(name),
            rm = quote(&rmeasures),
            gate = quote(&format!("{d}/regate")),
            t = timeout.as_secs(),
            l = quote(&rlog),
            m = quote(&format!("{log_dir}/{name}.started")),
            c = cmd.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" "),
        );
        let line = self.sh(&script)?;
        let outcome = parse_outcome(line.lines().last().unwrap_or("")).ok_or(format!("{}: no outcome from measure: {line}", self.dest))?;
        crate::sh_out(
            self.rsync()
                .args(["--remove-source-files", "--ignore-missing-args"])
                .arg(format!("{}:{out_dir}/{name}", self.dest))
                .arg(format!("{}/", local_out.display())),
        )
        .map_err(|e| format!("bringing {name} back: {e}"))?;
        crate::sh_out(self.rsync().args(["--remove-source-files", "--ignore-missing-args"]).arg(format!("{}:{rmeasures}", self.dest)).arg(measures))
            .map_err(|e| format!("bringing {name}'s measures back: {e}"))?;
        let text = self.sh(&format!("cat {l}; rm -rf {l} {o}", l = quote(&rlog), o = quote(&format!("{out_dir}/{name}"))))?;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(log).map_err(|e| format!("{}: {e}", log.display()))?;
        writeln!(f, "{text}").map_err(|e| e.to_string())?;
        Ok(outcome)
    }
}

/// `regate measure` prints this one line: what exec::run saw, tab-separated.
pub fn format_outcome(o: &Outcome) -> String {
    format!("outcome\t{}\t{}\t{}\t{}\t{}", o.status, o.wall_s, o.cpu_s, o.rss_mb, o.wait_s)
}

fn parse_outcome(line: &str) -> Option<Outcome> {
    let f: Vec<&str> = line.split('\t').collect();
    let [tag, status, wall, cpu, rss, wait] = f[..] else { return None };
    (tag == "outcome").then_some(())?;
    Some(Outcome { status: status.into(), wall_s: wall.parse().ok()?, cpu_s: cpu.parse().ok()?, rss_mb: rss.parse().ok()?, wait_s: wait.parse().ok()? })
}

/// One word for a POSIX shell.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conf_paths_and_outcome() {
        let r = Remote::parse("# where\nssh = -o IdentitiesOnly=yes -i /k user@10.0.0.1\nroot = /home/user/gate/\n").unwrap();
        assert_eq!((r.opts.len(), r.dest.as_str(), r.root.as_str()), (4, "user@10.0.0.1", "/home/user/gate"));
        assert_eq!(r.path(Path::new("/e"), Path::new("/e/data/test/a b.dat")).unwrap(), "/home/user/gate/data/test/a b.dat");
        assert!(r.path(Path::new("/e"), Path::new("/tmp/x.dat")).is_err());
        assert!(Remote::parse("root = rel\nssh = h").is_err());
        assert!(Remote::parse("ssh = h\nport = 2").is_err());
        let o = Outcome { status: "signal 9 (SIGKILL: memory cap or OOM)".into(), wall_s: 1.5, cpu_s: 2.25, rss_mb: 300.0, wait_s: 0.0 };
        let back = parse_outcome(&format_outcome(&o)).unwrap();
        assert_eq!((back.status, back.wall_s, back.cpu_s, back.rss_mb), (o.status, 1.5, 2.25, 300.0));
        assert_eq!(quote("it's"), "'it'\\''s'");
    }
}
