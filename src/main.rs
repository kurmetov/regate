//! regate (regression gate): build a project once per source tree (as its gate.conf says), run a fixed set of cases under the
//! machine's memory rules, keep every result in a history file, and compare.

mod build;
mod cases;
mod exec;
mod guard;
mod history;
mod init;
mod project;
mod queue;
mod remote;
mod remote_build;

use build::Bin;
use cases::Case;
use history::Rec;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// reproducible-builds.org's variable: a program that stamps its output with the time writes this instead,
/// so two runs of one binary can agree byte for byte.
const SOURCE_DATE_EPOCH: &str = "1767225600";

const USAGE: &str = "\
usage:
  regate init                            write gate.conf, the case file and a limits.conf sized to this machine
  regate build [REV]                     build REV (default: the working tree), print the binary
  regate run [SET] [--bin REV] [opts]    run a set of cases, record them, judge against history
  regate diff A B [SET] [opts]           run both, list the cases whose output or cost moved
  regate report [--case NAME] [--last N] crashes, slowdowns and memory growth over history
  regate list [SET]                      the cases of a set, and which this machine skips
  regate queue [--kind K] [-m 12G] [-t N] [--user U] -- CMD...
                                         wait for a place under <data>/limits.conf, then run CMD capped
  regate queue status                    what runs and what waits
  regate queue cancel ID [--force]       take a waiting job of this session (--force: of anyone) off the queue
  regate guard                           Claude Code PreToolUse hook: refuses a Bash command that breaks this
                                         machine's rules (bare cargo, git add -A in the shared root, ...)

REV is a commit, a branch, `tree` (the checkout you are in, a worktree too, uncommitted files included) or a path
to a binary. SET defaults to the first set in the case file.
opts: --only a,b   -j N (parallel cases, default 6; the queue still holds the limits)   --keep (keep outputs)
      --fresh (ignore cached results)   --threads N (CPU threads per case instead of its kind's job_threads;
      output that differs from another thread count of the same binary is reported and fails the run)
      --local (run here even when <data>/remote.conf sends cases to another machine)
A queued `cargo test|clippy|check` (--kind build) runs on remote.conf's machine too, the tree sent by rsync;
REGATE_BUILD_LOCAL=1 keeps it here.
Everything lives under <data> = <project>/.regate (data/gate or data/regate where one is there from before); the project
root is found from the current directory or REGATE_ROOT.
<project>/gate.conf (or <data>/gate.conf) says how to build it and where the binary and the case file are
(keys build, bin, cases, env, tag, check, stale, volatile, measures, beside, inputs; see the README).";

pub struct Ctx {
    pub root: PathBuf,
    pub data: PathBuf,
    pub project: project::Project,
    /// Where cases run when <data>/remote.conf names a machine and --local is not given.
    pub remote: Option<remote::Remote>,
}

impl Ctx {
    pub fn history(&self) -> PathBuf {
        self.data.join("history.jsonl")
    }
}

#[derive(Default)]
struct Opts {
    pos: Vec<String>,
    bin: Option<String>,
    only: Option<Vec<String>>,
    jobs: usize,
    keep: bool,
    fresh: bool,
    case: Option<String>,
    last: usize,
    threads: Option<u64>,
    local: bool,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match real_main(&args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("regate: {e}");
            2
        }
    };
    std::process::exit(code);
}

fn real_main(args: &[String]) -> Result<i32, String> {
    let Some((cmd, rest)) = args.split_first() else {
        eprintln!("{USAGE}");
        return Ok(2);
    };
    if matches!(cmd.as_str(), "-h" | "--help" | "help") {
        println!("{USAGE}");
        return Ok(0);
    }
    if cmd == "queue" {
        return queue_main(rest);
    }
    if cmd == "guard" {
        return Ok(guard::main());
    }
    if cmd == "init" {
        let root = context_root()?;
        let data = data_dir(&root);
        make_data_dir(&data)?;
        return init::main(&root, &data);
    }
    if cmd == "measure" {
        return measure_main(rest);
    }
    let o = parse_opts(rest)?;
    let mut ctx = context()?;
    if !o.local && matches!(cmd.as_str(), "run" | "diff") {
        ctx.remote = remote::Remote::load(&ctx.data)?;
        if let Some(r) = &ctx.remote {
            eprintln!("cases run on {} ({}/remote.conf; --local runs them here)", r.dest, ctx.data.display());
        }
    }
    make_data_dir(&ctx.data)?;
    match cmd.as_str() {
        "build" => {
            let bin = build::resolve(&ctx, o.pos.first().map(String::as_str))?;
            println!("{}", bin.path.display());
            Ok(0)
        }
        "run" => {
            let bin = build::resolve(&ctx, o.bin.as_deref())?;
            let (set, cases) = load_cases(&ctx, o.pos.first(), &o.only)?;
            println!("binary {} ({}), set [{set}], {} cases", &bin.md5[..10], short(&bin.rev), cases.len());
            let results = run_cases(&ctx, &bin, &set, &cases, &o);
            let hist = history::load(&ctx.history());
            let mut bad = 0;
            for (case, rec, cached) in &results {
                let mut verdict = match history::baseline(&hist, rec) {
                    _ if history::unjudged(&rec.status) => String::new(),
                    Some(b) => {
                        let words = history::verdict(rec, &b, (case.slower_pct, case.grow_pct));
                        bad += history::worse(&words) as i32;
                        words.join(", ")
                    }
                    None => "first run".into(),
                };
                let other = hist.iter().find(|r| r.key == rec.key && r.name == rec.name && r.status == "ok" && r.threads != rec.threads && r.out != rec.out);
                if let (Some(other), "ok") = (other, rec.status.as_str()) {
                    verdict = format!("OUTPUT DEPENDS ON THREADS: {} differs from {}. {verdict}", rec.threads, other.threads);
                    bad += 1;
                }
                bad += (rec.status != "ok" && !history::unjudged(&rec.status)) as i32;
                println!("{}  {}", line(case, rec, *cached), verdict);
            }
            Ok((bad > 0) as i32)
        }
        "diff" => {
            let [a, b, set @ ..] = o.pos.as_slice() else { return Err("diff needs A and B".into()) };
            let (a, b) = (build::resolve(&ctx, Some(a))?, build::resolve(&ctx, Some(b))?);
            let (set, cases) = load_cases(&ctx, set.first(), &o.only)?;
            if a.md5 == b.md5 {
                eprintln!("note: A and B are the same binary ({}), the comparison compares nothing", &a.md5[..10]);
            }
            let ra = run_cases(&ctx, &a, &set, &cases, &o);
            let rb = run_cases(&ctx, &b, &set, &cases, &o);
            println!("A {} ({})  B {} ({}), set [{set}]", &a.md5[..10], short(&a.rev), &b.md5[..10], short(&b.rev));
            println!("{:<22} {:<12} {:<12} {:<9} {:>16} {:>16}", "case", "A", "B", "output", "cpu s A→B", "peak GB A→B");
            let (mut moved, mut lost) = (0, 0);
            for ((case, x, _), (_, y, _)) in ra.iter().zip(&rb) {
                let output = if history::unjudged(&x.status) && history::unjudged(&y.status) {
                    "not run"
                } else if x.status != "ok" || y.status != "ok" {
                    "-"
                } else if x.out == y.out {
                    "same"
                } else {
                    "DIFFERS"
                };
                moved += (output != "same" && output != "not run") as i32;
                println!(
                    "{:<22} {:<12} {:<12} {:<9} {:>16} {:>16}",
                    case.name,
                    clip(&x.status, 12),
                    clip(&y.status, 12),
                    output,
                    format!("{:.1}→{:.1}", x.cpu_s, y.cpu_s),
                    format!("{:.2}→{:.2}", x.rss_mb / 1024.0, y.rss_mb / 1024.0),
                );
                if output == "DIFFERS" {
                    for f in changed_files(x, y) {
                        println!("    {f}");
                    }
                }
                // A differing output may be meant; a reference held worse is not, so it is counted on its own.
                if x.status == "ok" && y.status == "ok" {
                    let words = history::accuracy(&y.measures, &x.measures);
                    lost += history::worse(&words) as i32;
                    for w in words {
                        println!("    {w}");
                    }
                }
            }
            println!("{} of {} cases differ or failed", moved, cases.len());
            if lost > 0 {
                println!("{lost} of {} cases hold a reference worse in B (LESS ACCURATE or MEASURE GONE above)", cases.len());
            }
            Ok((moved + lost > 0) as i32)
        }
        "list" => {
            let (set, cases) = load_cases(&ctx, o.pos.first(), &o.only)?;
            let missing = |c: &Case| c.inputs.iter().find(|p| !p.exists()).map(|p| format!("  skipped: no {}", p.display()));
            for c in &cases {
                println!("{:<40} {}{}", c.name, c.args.join(" "), missing(c).unwrap_or_default());
            }
            let skipped = cases.iter().filter(|c| missing(c).is_some()).count();
            println!("[{set}]: {} cases, {skipped} skipped on this machine", cases.len());
            Ok(0)
        }
        "report" => {
            report(&ctx, &o);
            Ok(0)
        }
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
        }
    }
}

/// The queue has its own arguments: everything after them (or after `--`) is the command.
fn queue_main(args: &[String]) -> Result<i32, String> {
    let mut config: Option<PathBuf> = env("QUEUE_CONFIG").map(PathBuf::from);
    let mut job = queue::Job {
        config: PathBuf::new(),
        kind: "heavy".into(),
        mem_mb: None,
        threads: env("QUEUE_THREADS").and_then(|t| t.to_str()?.parse().ok()),
        user: env("QUEUE_USER").and_then(|u| u.into_string().ok()),
        no_scope: false,
        cmd: vec![],
    };
    let mut it = args.iter();
    let mut status = false;
    let mut cancel: Option<u64> = None;
    let mut force = false;
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--config" => config = Some(PathBuf::from(val()?)),
            "--kind" => job.kind = val()?,
            "-m" => job.mem_mb = Some(queue::parse_mem(&val()?).ok_or("-m needs a size like 12G")?),
            "-t" => job.threads = Some(val()?.parse().map_err(|_| "-t needs a number")?),
            "--user" => job.user = Some(val()?),
            "--no-scope" => job.no_scope = true,
            "status" if job.cmd.is_empty() => status = true,
            "cancel" => cancel = Some(val()?.parse().map_err(|_| "cancel needs a job id")?),
            "--force" => force = true,
            "--" => {
                job.cmd = it.cloned().collect();
                break;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(0);
            }
            // Not taken for the command: a misspelt option would otherwise wait in the queue as one.
            s if s.starts_with('-') => return Err(format!("queue: unknown option {s} (see regate --help)")),
            _ => {
                job.cmd = std::iter::once(a.clone()).chain(it.cloned()).collect();
                break;
            }
        }
    }
    job.config = match config {
        Some(c) => c,
        None => data_dir(&context_root()?).join("limits.conf"),
    };
    // Absolute: lock paths in the shared state are built from it and read from other directories.
    job.config = std::fs::canonicalize(&job.config).unwrap_or(job.config);
    if status {
        queue::status(&job.config)?;
        return Ok(0);
    }
    if let Some(id) = cancel {
        queue::cancel(&job.config, id, job.user, force)?;
        return Ok(0);
    }
    if job.cmd.is_empty() {
        return Err("queue needs a command".into());
    }
    if job.kind == "build"
        && let Some(code) = remote_build::run(&job)?
    {
        return Ok(code);
    }
    Err(queue::run(job))
}

/// `measure --timeout S --log FILE --marker FILE -- CMD...`: what a remote case runs there. Prints one
/// outcome line; the case's own output goes to the log.
fn measure_main(args: &[String]) -> Result<i32, String> {
    let (mut timeout, mut log, mut marker) = (None, None, None);
    let mut it = args.iter();
    let cmd: Vec<String> = loop {
        let Some(a) = it.next() else { return Err("measure needs -- CMD".into()) };
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--timeout" => timeout = Some(val()?.parse::<u64>().map_err(|_| "--timeout needs seconds")?),
            "--log" => log = Some(PathBuf::from(val()?)),
            "--marker" => marker = Some(PathBuf::from(val()?)),
            "--" => break it.cloned().collect(),
            s => return Err(format!("measure: unknown {s}")),
        }
    };
    let (Some(timeout), Some(log), Some(marker), Some((prog, rest))) = (timeout, log, marker, cmd.split_first()) else {
        return Err("measure needs --timeout, --log, --marker and a command".into());
    };
    let file = std::fs::File::create(&log).map_err(|e| format!("{}: {e}", log.display()))?;
    let mut c = Command::new(prog);
    c.args(rest).stdin(Stdio::null()).stdout(file.try_clone().map_err(|e| e.to_string())?).stderr(file);
    let o = exec::run(&mut c, Some(&marker), Duration::from_secs(timeout)).map_err(|e| e.to_string())?;
    println!("{}", remote::format_outcome(&o));
    Ok(0)
}

fn parse_opts(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts { jobs: 6, last: 20, ..Default::default() };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned().ok_or(format!("{a} needs a value"));
        match a.as_str() {
            "--bin" => o.bin = Some(val()?),
            "--only" => o.only = Some(val()?.split(',').map(String::from).collect()),
            "-j" => o.jobs = val()?.parse().map_err(|_| "-j needs a number")?,
            "--case" => o.case = Some(val()?),
            "--last" => o.last = val()?.parse().map_err(|_| "--last needs a number")?,
            "--threads" => o.threads = Some(val()?.parse().map_err(|_| "--threads needs a number")?),
            "--keep" => o.keep = true,
            "--fresh" => o.fresh = true,
            "--local" => o.local = true,
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            _ => o.pos.push(a.clone()),
        }
    }
    o.jobs = o.jobs.max(1);
    Ok(o)
}

/// The main checkout even when called from a worktree: the inputs and the gate's data live there.
fn context() -> Result<Ctx, String> {
    let root = context_root()?;
    let project = project::Project::load(&root)?;
    if !root.join(&project.cases).is_file() {
        return Err(format!("{} has no {} (the case file; {} names another)", root.display(), project.cases.display(), project::FILE));
    }
    Ok(Ctx { data: data_dir(&root), root, project, remote: None })
}

/// REGATE_<name>.
pub fn env(name: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(format!("REGATE_{name}"))
}

/// Where the gate keeps everything: <root>/.regate, or data/gate or data/regate where a project has one from before.
pub fn data_dir(root: &Path) -> PathBuf {
    ["data/gate", "data/regate"].map(|d| root.join(d)).into_iter().find(|d| d.is_dir()).unwrap_or_else(|| root.join(".regate"))
}

fn make_data_dir(data: &Path) -> Result<(), String> {
    std::fs::create_dir_all(data).map_err(|e| format!("{}: {e}", data.display()))?;
    // Out of git: the working-tree snapshot (`add -A`) would take the gate's own binaries and history in.
    let ignore = data.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, "*\n").map_err(|e| format!("{}: {e}", ignore.display()))?;
    }
    Ok(())
}

fn context_root() -> Result<PathBuf, String> {
    Ok(match env("ROOT") {
        Some(r) => PathBuf::from(r),
        None => {
            let common = sh_out(Command::new("git").args(["rev-parse", "--path-format=absolute", "--git-common-dir"]))
                .map_err(|e| format!("not inside a git repository ({e}); set REGATE_ROOT"))?;
            PathBuf::from(common).parent().unwrap().to_path_buf()
        }
    })
}

fn load_cases(ctx: &Ctx, set: Option<&String>, only: &Option<Vec<String>>) -> Result<(String, Vec<Case>), String> {
    let path = ctx.root.join(&ctx.project.cases);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let sets = cases::parse(&text, &ctx.root, &ctx.project).map_err(|e| format!("{}: {e}", path.display()))?;
    let (name, all) = sets.get(set.map(String::as_str))?;
    let mut cases = all.to_vec();
    if let Some(only) = only {
        if let Some(missing) = only.iter().find(|n| !cases.iter().any(|c| &c.name == *n)) {
            return Err(format!("no case {missing} in [{name}]"));
        }
        cases.retain(|c| only.contains(&c.name));
    }
    Ok((name.to_string(), cases))
}

/// Runs the cases not already in history for this binary and inputs, in parallel.
/// Returns (case, record, cached) in the case order.
fn run_cases(ctx: &Ctx, bin: &Bin, set: &str, cases: &[Case], o: &Opts) -> Vec<(Case, Rec, bool)> {
    let hist = history::load(&ctx.history());
    let limits = std::fs::read_to_string(ctx.data.join("limits.conf")).ok().and_then(|t| queue::Limits::parse(&t).ok());
    let threads_of = |case: &Case| o.threads.unwrap_or_else(|| case_threads(limits.as_ref(), case));
    let slots: Vec<Mutex<Option<(Rec, bool)>>> = cases.iter().map(|_| Mutex::new(None)).collect();
    let mut todo = vec![];
    for (i, case) in cases.iter().enumerate() {
        // Not a failure and not history: an input this machine does not have.
        if let Some(missing) = case.inputs.iter().find(|p| !p.exists()) {
            let status = format!("skipped: no {}", missing.display());
            let rec = Rec { kind: "case".into(), name: case.name.clone(), status, ..Default::default() };
            *slots[i].lock().unwrap() = Some((rec, false));
            continue;
        }
        let key = case_key(bin, case);
        let hit = (!o.fresh)
            .then(|| hist.iter().rev().find(|r| r.kind == "case" && r.key == key && r.name == case.name && r.status == "ok" && r.threads == threads_of(case) && r.repeat == case.repeat && r.host == host_of(ctx, case)))
            .flatten();
        match hit {
            Some(r) => *slots[i].lock().unwrap() = Some((r.clone(), true)),
            None => todo.push((i, key)),
        }
    }
    if !todo.is_empty() {
        eprintln!("{} to run, {} cached ({})", todo.len(), cases.len() - todo.len(), &bin.md5[..10]);
    }
    // Longest first, by the last good wall time (an unknown case counts as longest): a run ends with its slowest
    // case, which in case-file order (one of 131 s among 14 s ones) started last.
    let mut wall: HashMap<&str, f64> = HashMap::new();
    for r in hist.iter().filter(|r| r.kind == "case" && r.status == "ok") {
        wall.insert(&r.name, r.wall_s);
    }
    let wall_of = |i: usize| wall.get(cases[i].name.as_str()).copied().unwrap_or(f64::MAX);
    todo.sort_by(|a, b| wall_of(b.0).total_cmp(&wall_of(a.0)));
    let mut remote_ok = false;
    if let Some(r) = &ctx.remote
        && todo.iter().any(|(i, _)| runs_remote(ctx, &cases[*i]))
    {
        let mut inputs: Vec<PathBuf> = todo.iter().map(|(i, _)| &cases[*i]).filter(|c| runs_remote(ctx, c)).flat_map(|c| c.inputs.clone()).collect();
        inputs.sort();
        inputs.dedup();
        match r.prepare(&ctx.root, &bin.path, &bin.md5, &inputs) {
            Ok(()) => remote_ok = true,
            // No quiet fallback to this machine: the run says the other one failed.
            Err(e) => todo.retain(|(i, key)| {
                let case = &cases[*i];
                if !runs_remote(ctx, case) {
                    return true;
                }
                let rec = Rec { kind: "case".into(), name: case.name.clone(), key: key.clone(), status: format!("gate error: remote: {e}"), ..Default::default() };
                *slots[*i].lock().unwrap() = Some((rec, false));
                false
            }),
        }
    }
    let next = AtomicUsize::new(0);
    let hist_lock = Mutex::new(());
    let run_id = format!("{}-{}", now(), std::process::id());
    let started = std::time::Instant::now();
    std::thread::scope(|s| {
        for _ in 0..o.jobs.min(todo.len()) {
            s.spawn(|| {
                while let Some((i, key)) = todo.get(next.fetch_add(1, Ordering::SeqCst)) {
                    let case = &cases[*i];
                    let remote = ctx.remote.as_ref().filter(|_| remote_ok && runs_remote(ctx, case));
                    let rec = run_one(ctx, remote, bin, set, case, key, o.keep, threads_of(case), case_kind(limits.as_ref(), case), &run_id);
                    {
                        let _g = hist_lock.lock().unwrap();
                        if let Err(e) = history::append(&ctx.history(), &rec) {
                            eprintln!("history: {e}");
                        }
                    }
                    eprintln!("  {}", line(&cases[*i], &rec, false));
                    *slots[*i].lock().unwrap() = Some((rec, false));
                }
            });
        }
    });
    let results: Vec<(Case, Rec, bool)> = cases
        .iter()
        .zip(slots)
        .map(|(c, s)| {
            let (r, cached) = s.into_inner().unwrap().unwrap();
            (c.clone(), r, cached)
        })
        .collect();
    let ran = results.iter().filter(|(_, r, cached)| !cached && r.run == run_id);
    if let Some((_, worst, _)) = ran.max_by(|a, b| a.1.wait_s.total_cmp(&b.1.wait_s)) {
        eprintln!("{} ran in {:.1} min, longest queue wait {:.1} min ({})", todo.len(), started.elapsed().as_secs_f64() / 60.0, worst.wait_s / 60.0, worst.name);
    }
    results
}

/// Same binary, same arguments, same input files (by size and mtime): same result. The arguments hold `$out`
/// unexpanded, so two cases differing only in their output's name share a key: a hit also matches the name.
fn case_key(bin: &Bin, case: &Case) -> String {
    let mut s = format!("{}\n{}\n", bin.md5, case.args.join("\u{1f}"));
    for p in &case.inputs {
        let (len, mtime) = std::fs::metadata(p)
            .map(|m| (m.len(), m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())))
            .unwrap_or((0, None));
        s += &format!("{} {len} {:?}\n", p.display(), mtime);
    }
    md5_str(&s)
}

/// A case whose cap fits a [light] job has no reason to wait behind heavy ones.
fn case_kind(limits: Option<&queue::Limits>, case: &Case) -> &'static str {
    let light = limits.and_then(|l| l.kinds.get("light")?.job_mem_mb);
    if light.is_some_and(|l| queue::parse_mem(&case.mem).is_some_and(|m| m <= l)) { "light" } else { "heavy" }
}

/// What a case runs on unless --threads says otherwise: the queue's default for its kind.
fn case_threads(limits: Option<&queue::Limits>, case: &Case) -> u64 {
    limits.and_then(|l| l.kinds.get(case_kind(limits, case))?.job_threads).unwrap_or(1)
}

/// A failed case whose log holds `stale` (gate.conf: what a case prints when its input is not the one it was
/// recorded against, a recording replayed on a changed file) checked nothing: the input is to be recorded again, or the reader is wrong.
fn stale_status(status: &str, log: &Path, stale: &str) -> Option<String> {
    let log = std::fs::read(log).ok()?;
    (!stale.is_empty() && status.starts_with("exit ") && String::from_utf8_lossy(&log).contains(stale)).then(|| "stale: kept of another input".to_string())
}

/// A case goes to the other machine when every input it reads is an argument under the project
/// root: a JSON input naming more inputs names them by this machine's path, so it stays here.
fn runs_remote(ctx: &Ctx, case: &Case) -> bool {
    ctx.remote.is_some()
        && case.inputs.iter().all(|p| p.starts_with(&ctx.root) && case.args.iter().any(|a| Path::new(a) == p || cases::beside(Path::new(a), &ctx.project.beside).iter().any(|b| b == p)))
}

/// Where a case runs, as history records it. Outputs agree across machines (byte for byte),
/// times do not: an idle box took 11.5 s of CPU where this one took 19.3, so a cached result and a
/// baseline come from the same machine.
fn host_of(ctx: &Ctx, case: &Case) -> String {
    ctx.remote.as_ref().filter(|_| runs_remote(ctx, case)).map(|r| r.dest.clone()).unwrap_or_default()
}

#[allow(clippy::too_many_arguments)]
fn run_one(ctx: &Ctx, remote: Option<&remote::Remote>, bin: &Bin, set: &str, case: &Case, key: &str, keep: bool, threads: u64, kind: &str, run_id: &str) -> Rec {
    let begun = std::time::Instant::now();
    let out_dir = ctx.data.join("out").join(&bin.md5);
    let log_dir = ctx.data.join("logs").join(&bin.md5);
    let log_path = log_dir.join(format!("{}.log", case.name));
    let measures_path = log_dir.join(format!("{}.measures.jsonl", case.name));
    let mut rec = Rec {
        ts: now(),
        kind: "case".into(),
        name: case.name.clone(),
        set: set.into(),
        bin: bin.md5.clone(),
        rev: bin.rev.clone(),
        key: key.into(),
        load: loadavg(),
        log: log_path.to_string_lossy().into_owned(),
        threads,
        repeat: case.repeat,
        run: run_id.into(),
        host: remote.map(|r| r.dest.clone()).unwrap_or_default(),
        ..Default::default()
    };
    let result = (|| -> Result<(), String> {
        std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
        std::fs::create_dir_all(&log_dir).map_err(|e| e.to_string())?;
        // A binary's cases share this directory and their log names: a second run removed the first's output,
        // and a good run read as `ok but wrote nothing`. Held till this closure ends, gone with the process.
        let turn = std::fs::File::create(out_dir.join(format!("{}.lock", case.name))).map_err(|e| e.to_string())?;
        if unsafe { libc::flock(turn.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            eprintln!("  {}: another run of this binary has it, waiting", case.name);
            if unsafe { libc::flock(turn.as_raw_fd(), libc::LOCK_EX) } != 0 {
                return Err(format!("flock: {}", std::io::Error::last_os_error()));
            }
        }
        let out = out_dir.join(&case.name);
        remove(&out);
        // Truncated once; every repeat appends to it.
        std::fs::File::create(&log_path).map_err(|e| e.to_string())?;
        // The output is named relative to the run directory: a file that records its own path agrees across runs.
        let args = case.args.iter().map(|a| if a == "$out" { case.name.clone() } else { a.clone() });
        // Not current_exe(): once `cargo install` renames over this binary, that path ends in " (deleted)".
        let mut cmd = Command::new("/proc/self/exe");
        cmd.arg("queue")
            .arg("--config")
            .arg(ctx.data.join("limits.conf"))
            .args(["--kind", kind, "-m", &case.mem, "--"])
            .arg(&bin.path)
            .args(args)
            .current_dir(&out_dir)
            .env("SOURCE_DATE_EPOCH", SOURCE_DATE_EPOCH)
            .env("REGATE_QUEUE_THREADS", threads.to_string())
            .env("REGATE_MEASURES", &measures_path)
            .stdin(Stdio::null());
        if let Some(var) = &ctx.project.measures {
            cmd.env(var, &measures_path);
        }
        let marker = log_path.with_extension("started");
        // The same command in that machine's paths: its gate, its limits, its copy of the binary and inputs.
        let remote_cmd = match remote {
            Some(r) => {
                let gate = format!("{}/regate", r.data);
                let limits = format!("{}/limits.conf", r.data);
                let mut c = vec![gate, "queue".into(), "--config".into(), limits, "--kind".into(), kind.into(), "-m".into(), case.mem.clone(), "--".into(), r.bin(&bin.md5, &bin.path)];
                for a in &case.args {
                    let p = Path::new(a);
                    c.push(if a == "$out" { case.name.clone() } else if p.starts_with(&ctx.root) { r.path(&ctx.root, p)? } else { a.clone() });
                }
                Some((r, c))
            }
            None => None,
        };
        // Repeats keep the least CPU, wall and peak: the run least disturbed by the rest of the box.
        for run in 0..case.repeat {
            remove(&out);
            remove(&measures_path);
            let log = std::fs::OpenOptions::new().append(true).open(&log_path).map_err(|e| e.to_string())?;
            cmd.stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log);
            let o = match &remote_cmd {
                Some((r, c)) => {
                    let env = [("SOURCE_DATE_EPOCH", SOURCE_DATE_EPOCH.to_string()), ("REGATE_QUEUE_THREADS", threads.to_string())];
                    r.run(&bin.md5, &case.name, &env, ctx.project.measures.as_deref(), c, Duration::from_secs(case.timeout_s), &out_dir, &log_path, &measures_path)?
                }
                None => exec::run(&mut cmd, Some(&marker), Duration::from_secs(case.timeout_s)).map_err(|e| e.to_string())?,
            };
            rec.cpu_runs.push(o.cpu_s);
            rec.wait_s += o.wait_s;
            if run == 0 {
                (rec.wall_s, rec.cpu_s, rec.rss_mb) = (o.wall_s, o.cpu_s, o.rss_mb);
            } else {
                (rec.wall_s, rec.cpu_s, rec.rss_mb) = (rec.wall_s.min(o.wall_s), rec.cpu_s.min(o.cpu_s), rec.rss_mb.min(o.rss_mb));
            }
            rec.status = stale_status(&o.status, &log_path, &ctx.project.stale).unwrap_or(o.status);
            rec.measures = read_measures(&measures_path)?;
            if rec.status != "ok" {
                break;
            }
            // A case without $out (a check) is judged by exit, time and memory alone.
            if case.args.iter().any(|a| a == "$out") {
                let files = hash_outputs(&out_dir, &case.name, &ctx.project.volatile)?;
                let listing: String = files.iter().map(|(f, n, m)| format!("{f}\t{n}\t{m}\n")).collect();
                let digest = md5_str(&listing);
                if files.is_empty() {
                    rec.status = "ok but wrote nothing".into();
                } else if run > 0 && digest != rec.out {
                    rec.status = format!("output differs between runs 1 and {}", run + 1);
                } else if let Some(why) = check_failed(&ctx.root, &ctx.project.check, &out_dir, &files) {
                    rec.status = format!("ok but check failed: {why}");
                }
                (rec.files, rec.out) = (files, digest);
                if rec.status != "ok" {
                    break;
                }
            }
        }
        if !keep {
            remove(&out);
        }
        Ok(())
    })();
    if let Err(e) = result {
        rec.status = format!("gate error: {e}");
    }
    rec.took_s = begun.elapsed().as_secs_f64();
    rec
}

/// What the program wrote to REGATE_MEASURES: one {"name", "value"} per line; none when it wrote nothing.
fn read_measures(path: &Path) -> Result<std::collections::BTreeMap<String, f64>, String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(Default::default()) };
    let mut out = std::collections::BTreeMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).map_err(|e| format!("{}: {e}: {line}", path.display()))?;
        let (Some(name), Some(value)) = (v["name"].as_str(), v["value"].as_f64()) else {
            return Err(format!("{}: not {{name, value}}: {line}", path.display()));
        };
        out.insert(name.to_string(), value);
    }
    Ok(out)
}

/// (path relative to the run dir, bytes, md5) of every file the case wrote, sorted. A JSON holding a
/// `volatile` key is counted and hashed without it, so a time taken does not read as another output.
fn hash_outputs(dir: &Path, name: &str, volatile: &[String]) -> Result<Vec<(String, u64, String)>, String> {
    let mut files = vec![];
    let mut stack = vec![PathBuf::from(name)];
    while let Some(rel) = stack.pop() {
        let full = dir.join(&rel);
        let Ok(meta) = std::fs::metadata(&full) else { continue };
        if meta.is_dir() {
            for e in std::fs::read_dir(&full).map_err(|e| e.to_string())? {
                stack.push(rel.join(e.map_err(|e| e.to_string())?.file_name()));
            }
        } else {
            files.push((rel.to_string_lossy().into_owned(), meta.len()));
        }
    }
    files.sort();
    let mut out = vec![];
    for chunk in files.chunks(256) {
        let text = sh_out(Command::new("md5sum").arg("--").args(chunk.iter().map(|(f, _)| f)).current_dir(dir))?;
        for ((f, n), l) in chunk.iter().zip(text.lines()) {
            out.push((f.clone(), *n, l.split_whitespace().next().unwrap_or("").to_string()));
        }
    }
    for (f, n, md5) in out.iter_mut().filter(|(f, ..)| f.ends_with(".json")) {
        if let Some(text) = without_volatile(&dir.join(&*f), volatile) {
            (*n, *md5) = (text.len() as u64, md5_str(&text));
        }
    }
    Ok(out)
}

/// The JSON at `path` with every `volatile` key taken out at any depth; None when it holds none or is no JSON.
fn without_volatile(path: &Path, volatile: &[String]) -> Option<String> {
    fn strip(v: &mut serde_json::Value, keys: &[String]) {
        match v {
            serde_json::Value::Object(m) => {
                m.retain(|k, _| !keys.contains(k));
                m.values_mut().for_each(|v| strip(v, keys));
            }
            serde_json::Value::Array(a) => a.iter_mut().for_each(|v| strip(v, keys)),
            _ => {}
        }
    }
    let bytes = std::fs::read(path).ok()?;
    let holds = |k: &String| bytes.windows(k.len() + 2).any(|w| w[0] == b'"' && w[w.len() - 1] == b'"' && &w[1..w.len() - 1] == k.as_bytes());
    if !volatile.iter().any(holds) {
        return None;
    }
    let mut v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    strip(&mut v, volatile);
    serde_json::to_string(&v).ok()
}

/// gate.conf `check` on a case's outputs: why it failed, or None. A run on an empty input exits 0 and hashes
/// fine; only the project knows what an empty output looks like. Such a run is no baseline.
fn check_failed(root: &Path, check: &[String], dir: &Path, files: &[(String, u64, String)]) -> Option<String> {
    let (prog, args) = check.split_first()?;
    let out = match Command::new(prog).args(args).args(files.iter().map(|f| dir.join(&f.0))).current_dir(root).output() {
        Ok(o) if o.status.success() => return None,
        Ok(o) => o,
        Err(e) => return Some(format!("{prog}: {e}")),
    };
    let text = String::from_utf8_lossy(&[out.stdout, out.stderr].concat()).into_owned();
    Some(text.lines().map(str::trim).find(|l| !l.is_empty()).map_or_else(|| out.status.to_string(), |l| l.chars().take(80).collect()))
}

fn changed_files(a: &Rec, b: &Rec) -> Vec<String> {
    let set = |r: &Rec| r.files.iter().map(|(f, n, m)| (f.clone(), *n, m.clone())).collect::<HashSet<_>>();
    let (sa, sb) = (set(a), set(b));
    let mut names: Vec<&String> = sa.symmetric_difference(&sb).map(|(f, _, _)| f).collect();
    names.sort();
    names.dedup();
    let total = names.len();
    let mut lines: Vec<String> = names.into_iter().take(10).cloned().collect();
    if total > 10 {
        lines.push(format!("... and {} more files", total - 10));
    }
    lines
}

fn report(ctx: &Ctx, o: &Opts) {
    let hist = history::load(&ctx.history());
    if hist.is_empty() {
        println!("no history yet at {}", ctx.history().display());
        return;
    }
    // Each case's own thresholds from the case file; defaults for builds and cases no longer in it.
    let mut th: std::collections::HashMap<String, (f64, f64)> = Default::default();
    if let Ok(sets) = std::fs::read_to_string(ctx.root.join(&ctx.project.cases)).map_err(|e| e.to_string()).and_then(|t| cases::parse(&t, &ctx.root, &ctx.project)) {
        for c in sets.sets.values().flatten() {
            th.insert(c.name.clone(), (c.slower_pct, c.grow_pct));
        }
    }
    if let Some(name) = &o.case {
        println!("{:<17} {:<10} {:<10} {:<28} {:>8} {:>8} {:>8} {:<10}", "when", "binary", "rev", "status", "wall s", "cpu s", "peak GB", "output");
        let rows: Vec<&Rec> = hist.iter().filter(|r| &r.name == name).collect();
        for r in &rows[rows.len().saturating_sub(o.last)..] {
            println!(
                "{:<17} {:<10} {:<10} {:<28} {:>8.1} {:>8.1} {:>8.2} {:<10}",
                when(r.ts),
                clip(&r.bin, 10),
                short(&r.rev),
                clip(&r.status, 28),
                r.wall_s,
                r.cpu_s,
                r.rss_mb / 1024.0,
                clip(&r.out, 10)
            );
        }
        return;
    }
    // Latest record of every case and of builds, judged against the binaries before it.
    let mut names: Vec<(&str, &str)> = vec![];
    for r in hist.iter().rev() {
        if !names.contains(&(r.kind.as_str(), r.name.as_str())) {
            names.push((&r.kind, &r.name));
        }
    }
    names.sort();
    // A case taken out of the case file is history, not news: its last failure would read as today's.
    let gone = names.iter().filter(|(k, n)| *k == "case" && !th.is_empty() && !th.contains_key(*n)).count();
    names.retain(|(k, n)| *k != "case" || th.is_empty() || th.contains_key(*n));
    println!("{:<6} {:<22} {:<17} {:<28} {:>8} {:>8} {:>7}  vs earlier binaries", "", "name", "last run", "status", "cpu s", "peak GB", "fails");
    for (kind, name) in names {
        let recs: Vec<&Rec> = hist.iter().filter(|r| r.kind == kind && r.name == name).collect();
        let last = recs[recs.len() - 1];
        let fails: Vec<&&Rec> = recs.iter().filter(|r| history::failed(&r.status)).collect();
        // Not builds: one rebuilds a crate, the next the whole tree, and nothing tells them apart.
        let verdict = match history::baseline(&hist, last) {
            Some(b) if last.status == "ok" && kind == "case" => history::verdict(last, &b, th.get(name).copied().unwrap_or((15.0, 10.0))).join(", "),
            _ => String::new(),
        };
        println!(
            "{:<6} {:<22} {:<17} {:<28} {:>8.1} {:>8.2} {:>7}  {}",
            kind,
            name,
            when(last.ts),
            clip(&last.status, 28),
            last.cpu_s,
            last.rss_mb / 1024.0,
            fails.len(),
            verdict
        );
    }
    if gone > 0 {
        println!("({gone} cases no longer in {} left out; --case NAME shows one)", ctx.project.cases.display());
    }
    let runs = history::runs(&hist, o.last);
    if !runs.is_empty() {
        println!("\nruns (latest {}): minutes start to end, longest queue wait", runs.len());
        for (ts, set, n, took, wait, case) in runs {
            println!("  {}  {set:<10} {n:>3} cases  {took:>6.1} min  wait {wait:>6.1} min  {case}", when(ts));
        }
    }
    let fails: Vec<&Rec> = hist.iter().filter(|r| history::failed(&r.status)).collect();
    if !fails.is_empty() {
        println!("\nfailures (latest {}):", o.last.min(fails.len()));
        for r in &fails[fails.len().saturating_sub(o.last)..] {
            println!("  {}  {:<22} {:<10} {}  {}", when(r.ts), r.name, clip(&r.bin, 10), r.status, r.log);
        }
    }
}

fn line(case: &Case, r: &Rec, cached: bool) -> String {
    format!(
        "{:<22} {:<28} wall {:>7.1}s  cpu {:>7.1}s  peak {:>6.2} GB  {}{}",
        case.name,
        clip(&r.status, 28),
        r.wall_s,
        r.cpu_s,
        r.rss_mb / 1024.0,
        clip(&r.out, 10),
        if cached { "  (cached)" } else { "" }
    )
}

fn remove(p: &Path) {
    let _ = std::fs::remove_dir_all(p).or_else(|_| std::fs::remove_file(p));
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn short(rev: &str) -> String {
    if rev.contains('/') { rev.to_string() } else { clip(rev, 10) }
}

pub fn sh_out(cmd: &mut Command) -> Result<String, String> {
    let o = cmd.output().map_err(|e| format!("{:?}: {e}", cmd.get_program()))?;
    if !o.status.success() {
        return Err(format!("{:?} {:?}: {}", cmd.get_program(), cmd.get_args().collect::<Vec<_>>(), String::from_utf8_lossy(&o.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())
}

pub fn md5_file(p: &Path) -> Result<String, String> {
    let out = sh_out(Command::new("md5sum").arg("--").arg(p))?;
    Ok(out.split_whitespace().next().unwrap_or("").to_string())
}

fn md5_str(s: &str) -> String {
    let mut child = Command::new("md5sum").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().expect("md5sum");
    child.stdin.take().unwrap().write_all(s.as_bytes()).expect("md5sum stdin");
    let out = child.wait_with_output().expect("md5sum");
    String::from_utf8_lossy(&out.stdout).split_whitespace().next().unwrap_or("").to_string()
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn loadavg() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()?.parse().ok())
        .unwrap_or(0.0)
}

pub fn mem_available_gb() -> f64 {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    text.lines()
        .find_map(|l| l.strip_prefix("MemAvailable:")?.trim().strip_suffix("kB")?.trim().parse::<f64>().ok())
        .map_or(0.0, |kb| kb / 1024.0 / 1024.0)
}

/// Local time, minutes.
fn when(ts: u64) -> String {
    let t = ts as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday, tm.tm_hour, tm.tm_min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_taken_is_not_another_output() {
        let dir = std::env::temp_dir().join(format!("gate-volatile-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("o")).unwrap();
        let keys = vec!["analysis_time_seconds".to_string(), "processing_time_seconds".to_string()];
        let hash = |text: &str| {
            std::fs::write(dir.join("o/r.json"), text).unwrap();
            hash_outputs(&dir, "o", &keys).unwrap()
        };
        let a = hash(r#"{"area":1,"analysis_time_seconds":9.87,"mto":{"processing_time_seconds":1.0}}"#);
        assert_eq!(a, hash(r#"{"area":1,"analysis_time_seconds":12.3,"mto":{"processing_time_seconds":2.5}}"#));
        assert_ne!(a, hash(r#"{"area":2,"analysis_time_seconds":9.87,"mto":{"processing_time_seconds":1.0}}"#));
        assert_ne!(hash_outputs(&dir, "o", &[]).unwrap(), a);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_case_kept_of_another_input_is_stale_and_a_failure() {
        let log = std::env::temp_dir().join(format!("gate-stale-{}.log", std::process::id()));
        let stale = "is not the input it was kept of".to_string();
        std::fs::write(&log, format!("a.dat {stale}: 3 elements\n")).unwrap();
        assert_eq!(stale_status("exit 1", &log, &stale).as_deref(), Some("stale: kept of another input"));
        assert_eq!(stale_status("signal 11 (SIGSEGV)", &log, &stale), None);
        assert_eq!(stale_status("exit 1", &log, ""), None);
        std::fs::write(&log, "panic\n").unwrap();
        assert_eq!(stale_status("exit 1", &log, &stale), None);
        std::fs::remove_file(log).unwrap();
        let r = |bin: &str, rev: &str, cpu_s| Rec { kind: "case".into(), name: "a".into(), status: "ok".into(), bin: bin.into(), rev: rev.into(), cpu_s, ..Default::default() };
        let hist = [r("old", "t1", 100.0), r("new", "t2.false-16-false", 50.0)];
        assert_eq!(history::baseline(&hist, &r("mine", "t3.false-16-false", 60.0)).unwrap().cpu_s, 50.0);
        assert!(history::baseline(&hist[..1], &r("mine", "t3.false-16-false", 60.0)).is_none(), "built otherwise: says nothing about CPU");
        assert!(history::unjudged("skipped: no a.dat") && !history::unjudged("exit 1"));
        assert!(!history::unjudged("stale: kept of another input"));
        assert!(history::failed("exit 1") && !history::failed("ok") && history::failed("stale: kept of another input"));
        assert!(!history::failed("gate error: No such file or directory (os error 2)"), "the tool failed, not the code");
    }

    #[test]
    fn a_case_small_enough_goes_to_the_light_lane() {
        let l = queue::Limits::parse("[heavy]\njob_memory = 12G\njob_threads = 16\n[light]\njob_memory = 1G\njob_threads = 4\n").unwrap();
        let parsed = cases::parse("[s]\nc x\n", Path::new("/"), &Default::default()).unwrap();
        let c = |mem: &str| Case { mem: mem.into(), ..parsed.sets["s"][0].clone() };
        assert_eq!((case_kind(Some(&l), &c("1G")), case_threads(Some(&l), &c("512M"))), ("light", 4));
        assert_eq!((case_kind(Some(&l), &c("2G")), case_threads(Some(&l), &c("2G"))), ("heavy", 16));
        assert_eq!(case_kind(None, &c("1G")), "heavy");
    }
}
