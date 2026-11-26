//! The machine-wide job queue: `regate queue [opts] -- CMD...` waits its turn under the
//! limits in a config file, then execs CMD in a systemd scope capped at its memory and CPU.
//!
//! State is one JSON file beside the config, read and written under an flock. A job is the
//! PID of the queue process itself: it execs into the job, so the entry stays valid while the
//! job runs and frees itself when the process dies, however it dies (checked by PID plus
//! start time, so a reused PID is not mistaken for it).
//!
//! Admission, in ticket order: a waiter that fits takes its place; one that does not is passed
//! over, so a small job does not wait behind a big one while it fits. Held back by the machine's
//! limits for `HOLD_AFTER_S` and the oldest such, it stops those behind it that would delay it, so it is not starved by
//! a stream of small ones: it fits once today's jobs end, with theirs still running, or they wait.
//! Past `STRICT_AFTER_S` nothing passes it but the light lane, so it does get in.
//! A job that has not had the room in `ROOM_WINDOW_S`, with what runs given back, holds nobody, or the
//! machine would idle for nothing; its reason says so, for its session to lower the cap.
//! Held back by its own kind's or user's limits, it never stops anyone.
//!
//! Memory for the machine is what it really has: a job starts if `MemAvailable`, less the caps
//! of jobs too young to have grown yet, leaves `min_available` even when it reaches its own
//! cap. Reservations overstate (a 12G job peaks at 3G) and miss whatever runs outside the
//! queue. On top of that `[total] max_caps` bounds the sum of running caps, since jobs can all
//! peak at once. `[total] memory` only bounds one job's cap; kinds and users still sum theirs.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Limits of one scope (total, a kind, a user). `None` is no limit.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct Limit {
    pub jobs: Option<u64>,
    pub mem_mb: Option<u64>,
    pub threads: Option<u64>,
}

/// One kind of job: its limit, and what one job of it gets unless told otherwise.
#[derive(Clone, Copy, Default, Debug)]
pub struct Kind {
    pub limit: Limit,
    pub job_mem_mb: Option<u64>,
    pub job_threads: Option<u64>,
    /// systemd kills the job's scope after this long (RuntimeMaxSec).
    pub max_runtime_s: Option<u64>,
    /// Share of CPU against other scopes when the box is busy (CPUWeight, systemd's default 100).
    pub cpu_weight: Option<u64>,
    /// Counts against its own limits only, not [total]: a lane for jobs too small to wait for
    /// the big ones. Its memory has to be left out of [total] by hand. min_available still holds.
    pub outside_total: bool,
}

#[derive(Default, Debug)]
pub struct Limits {
    pub total: Limit,
    /// Admit only while MemAvailable is at least this: the box runs more than queued jobs.
    pub min_available_mb: u64,
    /// Admit only while the config's filesystem has at least this free.
    pub min_disk_mb: u64,
    /// The caps of running jobs never sum past this, whatever MemAvailable says now: jobs well
    /// below their caps can all peak at once.
    pub max_caps_mb: Option<u64>,
    /// `[total] data_ext`: extensions that mark a job's data file, so runs on one file share their peaks.
    pub data_ext: Vec<String>,
    pub kinds: HashMap<String, Kind>,
    pub user: Limit,
    pub users: HashMap<String, Limit>,
    /// `[gate] keep_binaries`: built binaries the gate keeps (newest first); None keeps all.
    pub keep_binaries: Option<usize>,
    /// Keys this binary does not know: one written ahead of the binary must not stop the queue.
    pub unknown: Vec<String>,
}

impl Limits {
    pub fn parse(text: &str) -> Result<Limits, String> {
        let mut l = Limits::default();
        let mut section = String::new();
        let mut unknown = vec![];
        for (no, raw) in text.lines().enumerate() {
            let at = |m: String| format!("line {}: {m}", no + 1);
            let line = raw.split('#').next().unwrap().trim();
            if line.is_empty() {
                continue;
            }
            if let Some(s) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                section = s.trim().to_string();
                continue;
            }
            let (key, value) = line.split_once('=').ok_or_else(|| at("expected key = value".into()))?;
            let (key, value) = (key.trim(), value.trim());
            let num = |v: &str| v.parse::<u64>().map_err(|_| at(format!("{key}: {v} is not a number")));
            let mem = |v: &str| parse_mem(v).ok_or_else(|| at(format!("{key}: {v} is not a size like 12G")));
            let mut set = |lim: &mut Limit| -> Result<(), String> {
                match key {
                    "jobs" => lim.jobs = Some(num(value)?),
                    "memory" => lim.mem_mb = Some(mem(value)?),
                    "threads" => lim.threads = Some(num(value)?),
                    _ => unknown.push(at(format!("unknown key {key} in [{section}]"))),
                }
                Ok(())
            };
            match section.as_str() {
                "" => return Err(at("a key before any [section]".into())),
                "total" if key == "min_available" => l.min_available_mb = mem(value)?,
                "total" if key == "min_disk" => l.min_disk_mb = mem(value)?,
                "total" if key == "max_caps" => l.max_caps_mb = Some(mem(value)?),
                "total" if key == "data_ext" => l.data_ext = value.split_whitespace().map(str::to_lowercase).collect(),
                "total" => set(&mut l.total)?,
                "user" => set(&mut l.user)?,
                s if s.starts_with("user.") => set(l.users.entry(s[5..].to_string()).or_default())?,
                "gate" if key == "keep_binaries" => l.keep_binaries = Some(num(value)? as usize),
                "gate" => unknown.push(at(format!("unknown key {key} in [gate]"))),
                kind => {
                    let k = l.kinds.entry(kind.to_string()).or_default();
                    match key {
                        "job_memory" => k.job_mem_mb = Some(mem(value)?),
                        "job_threads" => k.job_threads = Some(num(value)?),
                        "max_runtime" => {
                            k.max_runtime_s = Some(parse_secs(value).ok_or_else(|| at(format!("{key}: {value} is not a time like 2h")))?)
                        }
                        "cpu_weight" => k.cpu_weight = Some(num(value)?.clamp(1, 10000)),
                        "outside_total" => k.outside_total = value == "yes",
                        _ => set(&mut k.limit)?,
                    }
                }
            }
        }
        l.unknown = unknown;
        Ok(l)
    }

    /// A user's limit: `[user.NAME]` whose NAME the user starts with, else `[user]`.
    fn user_limit(&self, user: &str) -> Limit {
        let named = self.users.iter().filter(|(n, _)| user.starts_with(n.as_str())).max_by_key(|(n, _)| n.len());
        named.map_or(self.user, |(_, l)| *l)
    }
}

pub fn parse_mem(v: &str) -> Option<u64> {
    let v = v.trim();
    let (n, mul) = match v.chars().last()? {
        'G' | 'g' => (&v[..v.len() - 1], 1024),
        'M' | 'm' => (&v[..v.len() - 1], 1),
        _ => (v, 1),
    };
    Some((n.parse::<f64>().ok()? * mul as f64) as u64)
}

/// `90`, `90s`, `30m`, `2h` in seconds.
pub fn parse_secs(v: &str) -> Option<u64> {
    let v = v.trim();
    let (n, mul) = match v.chars().last()? {
        's' => (&v[..v.len() - 1], 1),
        'm' => (&v[..v.len() - 1], 60),
        'h' => (&v[..v.len() - 1], 3600),
        _ => (v, 1),
    };
    Some((n.parse::<f64>().ok()? * mul as f64) as u64)
}

/// Free space on the filesystem holding `p`, in MB.
pub fn disk_free_mb(p: &Path) -> Option<u64> {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut s) } == 0).then(|| s.f_bavail as u64 * s.f_frsize as u64 / (1024 * 1024))
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub id: u64,
    /// The queue process, then the job it became; shown, not trusted for liveness.
    pub pid: i32,
    /// The job's lock file: held for as long as any process of the job lives.
    pub lock: PathBuf,
    pub user: String,
    pub kind: String,
    pub mem_mb: u64,
    pub threads: u64,
    pub running: bool,
    /// Unix seconds it queued, then when it started.
    pub since: u64,
    pub cmd: String,
}

#[derive(Default, Clone, Copy)]
struct Use {
    jobs: u64,
    mem_mb: u64,
    threads: u64,
}

impl Use {
    fn add(&mut self, e: &Entry) {
        self.jobs += 1;
        self.mem_mb += e.mem_mb;
        self.threads += e.threads;
    }
    fn fits(&self, e: &Entry, l: &Limit) -> bool {
        l.jobs.is_none_or(|n| self.jobs < n)
            && l.mem_mb.is_none_or(|n| self.mem_mb + e.mem_mb <= n)
            && l.threads.is_none_or(|n| self.threads + e.threads <= n)
    }
}

/// The numbers in a reason move on every look: only a new kind of reason is worth a line in the job's log.
/// Whether the disk lets any job start: below `min_disk` nothing does, whatever the memory says.
/// The admission loop and `status` both ask this before [`decide`], so `status` names what a job
/// really waits for: otherwise it said "N MB free" of memory while the queue stood for want of disk.
fn disk_stop(limits: &Limits, disk_free_mb: Option<u64>) -> Result<(), String> {
    match disk_free_mb {
        Some(free) if free < limits.min_disk_mb => Err(format!(
            "{:.0} GB of disk free, below min_disk {:.0} GB",
            free as f64 / 1024.0,
            limits.min_disk_mb as f64 / 1024.0
        )),
        _ => Ok(()),
    }
}

fn same_reason(a: &str, b: &str) -> bool {
    let words = |s: &str| s.chars().filter(|c| !c.is_ascii_digit()).collect::<String>();
    words(a) == words(b)
}

/// A started job's memory does not show in MemAvailable until it has grown: this long, its
/// whole cap counts as taken. Only to keep jobs from starting on the same room: peaking all at
/// once is max_caps' to bound. 60 s held starts to two a minute with 24 GB free; a job touching
/// memory as fast as it can grew 2.8 GB in 14 s.
const RAMP_S: u64 = 20;

/// How long the oldest job the machine cannot hold yet lets others past it. 5 minutes stood five
/// jobs behind a 12G one with 20 GB free and two jobs running of eight.
const HOLD_AFTER_S: u64 = 1200;

/// Held this long and still not in, nothing passes it but the light lane: the running jobs end and
/// no new ones come, so it gets in. 8G jobs passing a 14G one kept the caps too full for it.
const STRICT_AFTER_S: u64 = HOLD_AFTER_S + 1200;

/// A job this small passes any holder: a 14G one kept 17 jobs standing, most of them tests under 2 GB, with one job running.
/// shortcut: a stream of small jobs can starve a big one; its owner sees that in `status`, lower its -m or quiet the machine.
const SMALL_JOB_MB: u64 = 4096;

/// A job holds if the room it needs was there this recently, with what runs given back: `MemAvailable`
/// moves with what runs outside the queue. Judged on the moment, a 24G job needing 32 GB flickered in and
/// out of holding at 32.5 and waited 1 h 44 min; with 2 GB of slack, another at 31.8 held four jobs behind it for
/// an hour and a half on an empty machine.
const ROOM_WINDOW_S: u64 = 600;
/// One line of the record per this long, holding the most seen in it.
const ROOM_STEP_S: u64 = 10;
/// The record beside the state: not in queue.json, whose format clients already waiting still read.
const ROOM: &str = "queue-room.txt";

/// Records `alone_mb` (`MemAvailable` + running anon) and returns the most reached in the window.
/// None until the window is on record without a hole: an unseen minute may hold the peak.
fn note_room(path: &Path, alone_mb: u64, now: u64) -> Option<u64> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let line = |l: &str| {
        let (t, mb) = l.split_once(' ')?;
        Some((t.parse::<u64>().ok()?, mb.parse::<u64>().ok()?))
    };
    let mut seen: Vec<(u64, u64)> = text.lines().filter_map(line).filter(|(t, _)| now.saturating_sub(*t) <= ROOM_WINDOW_S + 2 * ROOM_STEP_S).collect();
    match seen.last_mut() {
        Some((t, mb)) if now.saturating_sub(*t) < ROOM_STEP_S => *mb = alone_mb.max(*mb),
        _ => seen.push((now, alone_mb)),
    }
    // The queue's lock is held. A job does not fail for want of a line here.
    let out: String = seen.iter().map(|(t, mb)| format!("{t} {mb}\n")).collect();
    let _ = std::fs::write(path, out);
    let covered = seen[0].0 + ROOM_WINDOW_S <= now && seen.windows(2).all(|p| p[1].0 - p[0].0 <= 6 * ROOM_STEP_S);
    covered.then(|| seen.iter().map(|s| s.1).max().unwrap_or(0))
}

/// A running job's anonymous memory and its cgroup's peak, MB. Nothing for a job gone from /proc,
/// or not in its own scope yet: until systemd moves it there it sits in its caller's, and the
/// 10.6 GB of the desktop app it was started from read as room that running jobs would give back.
fn job_memory_mb(pid: i32) -> Option<(u64, u64)> {
    let cgroup = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let cgroup = cgroup.trim().strip_prefix("0::")?;
    if !cgroup.contains(&format!("/run-p{pid}-")) {
        return None;
    }
    let dir = format!("/sys/fs/cgroup{cgroup}");
    let stat = std::fs::read_to_string(format!("{dir}/memory.stat")).ok()?;
    let peak = std::fs::read_to_string(format!("{dir}/memory.peak")).ok()?.trim().parse::<u64>().ok()?;
    Some((held_anon(&stat)? >> 20, peak >> 20))
}

/// Anonymous memory a job's end would give back, from its memory.stat: the anon lists, not `anon`. That
/// also counts pages the allocator gave up with MADV_FREE, which MemAvailable already has: with `anon`,
/// MemAvailable + anon ran 31.6-38.2 GB in five minutes, with the lists 31.4-32.4.
/// shortcut: the lists hold a job's tmpfs too, which outlives it; 0 in every job sampled, subtract `shmem` if that changes.
fn held_anon(stat: &str) -> Option<u64> {
    let list = |name: &str| stat.lines().find_map(|l| l.strip_prefix(name)?.strip_prefix(' ')?.parse::<u64>().ok());
    Some(list("inactive_anon")? + list("active_anon")?)
}

/// What MemAvailable gains when the running jobs end: their anonymous memory, not their caps
/// (a script under an 8G cap held 279 MB).
fn running_anon_mb(entries: &[Entry]) -> u64 {
    entries.iter().filter(|e| e.running).filter_map(|e| job_memory_mb(e.pid)).map(|m| m.0).sum()
}

/// Whether `me` may start now, and if not, what holds it. `avail_mb` is MemAvailable, `anon_mb`
/// what the running jobs would give back, `room_mb` what `note_room` returned.
fn decide(entries: &[Entry], me: u64, limits: &Limits, avail_mb: u64, anon_mb: u64, room_mb: Option<u64>, now: u64) -> Result<(), String> {
    let mut total = Use::default();
    let total_limit = Limit { mem_mb: None, ..limits.total };
    let mut budget = avail_mb;
    let mut kinds: HashMap<&str, Use> = HashMap::new();
    let mut users: HashMap<&str, Use> = HashMap::new();
    let outside = |e: &Entry| limits.kinds.get(&e.kind).is_some_and(|k| k.outside_total);
    let me_outside = entries.iter().any(|e| e.id == me && outside(e));
    for e in entries.iter().filter(|e| e.running) {
        if !outside(e) {
            total.add(e);
            if now.saturating_sub(e.since) < RAMP_S {
                budget = budget.saturating_sub(e.mem_mb);
            }
        }
        kinds.entry(&e.kind).or_default().add(e);
        users.entry(&e.user).or_default().add(e);
    }
    let mut waiting: Vec<&Entry> = entries.iter().filter(|e| !e.running).collect();
    waiting.sort_by_key(|e| e.id);
    let alone_mb = avail_mb + anon_mb;
    // The oldest job holding the queue, and the caps it keeps room for: its own and those of jobs that passed it.
    let mut held: Option<(&Entry, u64)> = None;
    for w in waiting {
        let kind_limit = limits.kinds.get(&w.kind).map_or(Limit::default(), |k| k.limit);
        // A lane is small enough to need only the floor; the rest must leave the floor at their cap.
        let fits_caps = outside(w) || limits.max_caps_mb.is_none_or(|c| total.mem_mb + w.mem_mb <= c);
        let fits_memory = if outside(w) { avail_mb >= limits.min_available_mb } else { budget >= w.mem_mb + limits.min_available_mb };
        let fits_total = total.fits(w, &total_limit) || outside(w);
        let fits_kind = kinds.get(w.kind.as_str()).copied().unwrap_or_default().fits(w, &kind_limit);
        let fits_user = users.get(w.user.as_str()).copied().unwrap_or_default().fits(w, &limits.user_limit(&w.user));
        // A job behind a holder passes if the holder still fits once today's jobs end with this one running.
        // shortcut: threads are not reserved, only memory and caps; jobs here take 16 threads each, so they cannot pass unbounded.
        let fits_hold = w.mem_mb <= SMALL_JOB_MB || held.is_none_or(|(h, mb)| {
            now.saturating_sub(h.since) < STRICT_AFTER_S && alone_mb >= mb + w.mem_mb + limits.min_available_mb && limits.max_caps_mb.is_none_or(|c| mb + w.mem_mb <= c)
        });
        let need_mb = w.mem_mb + limits.min_available_mb;
        if w.id == me {
            if let (false, Some(seen)) = (outside(w), room_mb.filter(|seen| *seen < need_mb)) {
                return Err(format!("will not get in: needs {need_mb} MB, at most {seen} MB in the last {} min: lower its -m", ROOM_WINDOW_S / 60));
            }
            if let (false, Some((h, _))) = (fits_hold, held) {
                return Err(format!("behind job {} ({} {}), waiting for the machine for {} s", h.id, h.kind, h.user, now.saturating_sub(h.since)));
            }
            return match (fits_total, fits_kind, fits_user) {
                (_, false, _) => Err(format!("[{}] limit", w.kind)),
                (_, _, false) => Err(format!("user {} limit", w.user)),
                (false, _, _) => {
                    let (jobs, threads) = (limits.total.jobs, limits.total.threads);
                    let on_threads = threads.is_some_and(|n| total.threads + w.threads > n);
                    Err(if on_threads {
                        format!("machine total limit: threads {}+{} of {}", total.threads, w.threads, threads.unwrap_or(0))
                    } else {
                        format!("machine total limit: jobs {} of {}", total.jobs, jobs.unwrap_or(0))
                    })
                }
                _ if !fits_caps => {
                    Err(format!("caps of running jobs {} MB, its {} MB would pass max_caps", total.mem_mb, w.mem_mb))
                }
                _ if !fits_memory => {
                    Err(format!("{budget} MB free, needs its {} MB above min_available {} MB", w.mem_mb, limits.min_available_mb))
                }
                _ => Ok(()),
            };
        }
        // Held by its own kind or user, it could not start even with the machine free: it holds nobody.
        if fits_kind && fits_user {
            if !fits_total || !fits_memory || !fits_caps || !fits_hold {
                // Only the oldest holds, and a job outside the total passes for good.
                if me_outside || held.is_some() || now.saturating_sub(w.since) < HOLD_AFTER_S {
                    continue;
                }
                // Holding is for the room the running jobs will give back. With none running, or too little to give, it would idle the machine for nothing.
                if total.jobs > 0 && room_mb.unwrap_or(alone_mb) >= need_mb {
                    let passed: u64 = entries.iter().filter(|e| e.running && !outside(e) && e.since >= w.since + HOLD_AFTER_S).map(|e| e.mem_mb).sum();
                    held = Some((w, w.mem_mb + passed));
                }
                continue;
            }
            // It starts on its next look; count it as started.
            if !outside(w) {
                total.add(w);
                budget = budget.saturating_sub(w.mem_mb);
            }
            kinds.entry(&w.kind).or_default().add(w);
            users.entry(&w.user).or_default().add(w);
        }
    }
    Err("not in the queue".into())
}

/// A job is alive while anything holds the flock on its file: the queue process takes it at
/// registration and it is inherited across exec and by every child, so the place is freed
/// only when the last process of the job is gone, however it ended. (A PID is not enough:
/// a job's shell can be gone from /proc while the work it started still runs.)
fn alive(e: &Entry) -> bool {
    let Ok(f) = File::open(&e.lock) else { return false };
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        let _ = std::fs::remove_file(&e.lock);
        return false;
    }
    true
}

/// Creates and locks the job's file, and keeps the descriptor open past exec.
fn hold(path: &Path) -> Result<(), String> {
    let f = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let fd = f.as_raw_fd();
    if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } != 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, 0) } != 0 {
        return Err(format!("{}: {}", path.display(), std::io::Error::last_os_error()));
    }
    std::mem::forget(f);
    Ok(())
}

struct State {
    path: PathBuf,
    _lock: File,
    entries: Vec<Entry>,
}

impl State {
    fn open(dir: &Path) -> Result<State, String> {
        let lock = File::create(dir.join("queue.lock")).map_err(|e| format!("{}: {e}", dir.display()))?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(format!("flock: {}", std::io::Error::last_os_error()));
        }
        let path = dir.join("queue.json");
        // A state that does not parse is an error, never an empty queue: that would admit everyone.
        let mut entries: Vec<Entry> = match std::fs::read_to_string(&path) {
            Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => vec![],
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        entries.retain(alive);
        // Marked started but still waiting in this program, past the moment of its exec: a client
        // of an older version took another's entry for its own. Left so, it holds a place for good.
        let now = crate::now();
        for e in entries.iter_mut().filter(|e| e.running && now.saturating_sub(e.since) > 5) {
            let args = std::fs::read(format!("/proc/{}/cmdline", e.pid)).unwrap_or_default();
            if args.split(|&b| b == 0).nth(1) == Some(b"queue".as_slice()) {
                e.running = false;
            }
        }
        Ok(State { path, _lock: lock, entries })
    }
    fn save(&self) -> Result<(), String> {
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self.entries).expect("plain struct serializes");
        std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, &self.path)).map_err(|e| e.to_string())
    }
}

pub struct Job {
    pub config: PathBuf,
    pub kind: String,
    pub mem_mb: Option<u64>,
    pub threads: Option<u64>,
    pub user: Option<String>,
    /// Run the command bare instead of in a systemd scope (the load test's fake jobs).
    pub no_scope: bool,
    pub cmd: Vec<String>,
}

/// Waits for a place, then execs; returns only on an error.
pub fn run(job: Job) -> String {
    match wait_and_exec(job) {
        Ok(never) => match never {},
        Err(e) => e,
    }
}

enum Never {}

fn wait_and_exec(job: Job) -> Result<Never, String> {
    let text = std::fs::read_to_string(&job.config).map_err(|e| format!("{}: {e}", job.config.display()))?;
    let mut limits = Limits::parse(&text).map_err(|e| format!("{}: {e}", job.config.display()))?;
    for u in &limits.unknown {
        eprintln!("regate queue: {}: {u}, ignored", job.config.display());
    }
    let kind = *limits.kinds.get(&job.kind).ok_or_else(|| format!("no [{}] in {}", job.kind, job.config.display()))?;
    let kind_limit = kind.limit;
    let mem_mb = job.mem_mb.or(kind.job_mem_mb).ok_or_else(|| format!("[{}] has no job_memory and no -m was given", job.kind))?;
    let threads = job.threads.or(kind.job_threads).unwrap_or(1).max(1);
    let user = job.user.clone().unwrap_or_else(default_user);
    let pid = std::process::id() as i32;
    let me = Entry {
        id: 0,
        pid,
        lock: PathBuf::new(),
        user: user.clone(),
        kind: job.kind.clone(),
        mem_mb,
        threads,
        running: false,
        since: crate::now(),
        cmd: job.cmd.join(" "),
    };
    // A job no limit can ever hold would wait forever: refuse it now.
    let total = if kind.outside_total { Limit::default() } else { limits.total };
    for (what, l) in [("machine total", total), (&*format!("[{}]", job.kind), kind_limit), ("user", limits.user_limit(&user))] {
        if !Use::default().fits(&me, &l) {
            return Err(format!("{mem_mb} MB / {threads} threads exceeds the {what} limit in {}", job.config.display()));
        }
    }
    let dir = job.config.parent().unwrap_or(Path::new("."));
    // From here `me.mem_mb` is what the job holds against the limits; `mem_mb` stays its kill cap.
    let me = Entry { mem_mb: past_runs(dir, true, &limits.data_ext).get(&job_key(&me.cmd, &limits.data_ext)).map_or(mem_mb, |r| reserve_mb(mem_mb, r)), ..me };
    let jobs = dir.join("jobs");
    std::fs::create_dir_all(&jobs).map_err(|e| format!("{}: {e}", jobs.display()))?;
    let (id, me) = {
        let mut st = State::open(dir)?;
        // Never reused: two jobs under one id marked each other's entry as started, and the
        // one left waiting held its place for good.
        let counter = dir.join("queue.next");
        let last: u64 = std::fs::read_to_string(&counter).ok().and_then(|t| t.trim().parse().ok()).unwrap_or(0);
        let id = last.max(st.entries.iter().map(|e| e.id).max().unwrap_or(0)) + 1;
        std::fs::write(&counter, id.to_string()).map_err(|e| format!("{}: {e}", counter.display()))?;
        let me = Entry { id, lock: jobs.join(format!("{id}-{pid}.lock")), ..me };
        hold(&me.lock)?;
        st.entries.push(me.clone());
        st.save()?;
        (id, me)
    };
    let queued = Instant::now();
    let mut last_said: Option<(Instant, String)> = None;
    loop {
        {
            // An edit of limits.conf reaches jobs already waiting (25.11: 30 jobs kept the old 5-slot limit);
            // a file caught mid-write keeps the last one that parsed. This job's own caps stay as it was queued.
            if let Some(l) = std::fs::read_to_string(&job.config).ok().and_then(|t| Limits::parse(&t).ok()) {
                limits = l;
            }
            let mut st = State::open(dir)?;
            if !st.entries.iter().any(|e| e.id == id && e.pid == pid) {
                st.entries.push(me.clone());
            }
            let avail_mb = (crate::mem_available_gb() * 1024.0) as u64;
            let (anon_mb, now) = (running_anon_mb(&st.entries), crate::now());
            let room_mb = note_room(&dir.join(ROOM), avail_mb + anon_mb, now);
            let verdict = disk_stop(&limits, disk_free_mb(dir)).and_then(|()| decide(&st.entries, id, &limits, avail_mb, anon_mb, room_mb, now));
            match verdict {
                Ok(()) => {
                    let e = st.entries.iter_mut().find(|e| e.id == id && e.pid == pid).unwrap();
                    e.running = true;
                    e.since = crate::now();
                    let started = e.clone();
                    st.save()?;
                    // For `status`' hints; a job does not fail for want of a line here.
                    if let Ok(mut f) = std::fs::File::options().create(true).append(true).open(dir.join(STARTS)) {
                        let _ = writeln!(f, "{}", serde_json::to_string(&started).expect("plain struct serializes"));
                    }
                    break;
                }
                Err(why) => {
                    st.save()?;
                    let say = last_said.as_ref().is_none_or(|(t, w)| !same_reason(w, &why) || t.elapsed() > Duration::from_secs(60));
                    if say {
                        eprintln!("queue: job {id} ({} {user}) waits {:.0} s: {why}", job.kind, queued.elapsed().as_secs_f64());
                        last_said = Some((Instant::now(), why));
                    }
                }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    let mut cmd = if job.no_scope {
        let mut c = Command::new(&job.cmd[0]);
        c.args(&job.cmd[1..]);
        c
    } else {
        let mut c = Command::new("systemd-run");
        c.args(["--user", "--scope", "-q"])
            .arg(format!("-pMemoryMax={mem_mb}M"))
            .arg("-pMemorySwapMax=0")
            .arg(format!("-pCPUQuota={}%", threads * 100));
        if let Some(s) = kind.max_runtime_s {
            c.arg(format!("-pRuntimeMaxSec={s}"));
        }
        if let Some(w) = kind.cpu_weight {
            c.arg(format!("-pCPUWeight={w}"));
        }
        c.args(&job.cmd);
        c
    };
    if job.kind == "build" && std::env::var_os("CARGO_BUILD_JOBS").is_none() {
        cmd.env("CARGO_BUILD_JOBS", threads.to_string());
    }
    cmd.env("REGATE_QUEUE_JOB", id.to_string());
    // A queue inside this job must not move this job's mark.
    cmd.env_remove(crate::exec::STARTED_ENV);
    crate::exec::mark_started();
    Err(format!("exec {}: {}", job.cmd[0], cmd.exec()))
}

/// The Claude session if this runs inside one (they all share the OS user), else the OS user.
pub fn default_user() -> String {
    if let Ok(s) = std::env::var("CLAUDE_CODE_SESSION_ID") {
        return format!("session-{}", &s[..s.len().min(8)]);
    }
    std::env::var("USER").unwrap_or_else(|_| "unknown".into())
}

/// Takes a waiting job off the queue: its client dies, of whatever version, and the next look at
/// the state drops its entry. A running job is its session's to stop, with its own kill.
pub fn cancel(config: &Path, id: u64, user: Option<String>, force: bool) -> Result<(), String> {
    let st = State::open(config.parent().unwrap_or(Path::new(".")))?;
    let e = st.entries.iter().find(|e| e.id == id).ok_or(format!("no job {id} in the queue"))?;
    if e.running {
        return Err(format!("job {id} is running, not waiting: stop it with a kill of pid {}", e.pid));
    }
    let user = user.unwrap_or_else(default_user);
    if e.user != user && !force {
        return Err(format!("job {id} belongs to {}, this is {user}: --force to cancel it anyway", e.user));
    }
    // A reused PID is not the client.
    let args = std::fs::read(format!("/proc/{}/cmdline", e.pid)).unwrap_or_default();
    if args.split(|&b| b == 0).nth(1) != Some(b"queue".as_slice()) || unsafe { libc::kill(e.pid, libc::SIGTERM) } != 0 {
        return Err(format!("job {id}: pid {} is not its waiting client, or would not take a signal", e.pid));
    }
    println!("job {id} cancelled: {}", e.cmd.chars().take(80).collect::<String>());
    Ok(())
}

/// Every job started, one `Entry` a line: the cap and command the journal does not keep.
const STARTS: &str = "queue-starts.jsonl";
/// `status` hints only on a peak under half the cap in all of this many runs in a row.
const HINT_RUNS: usize = 5;
/// Runs of a job's type before its reservation comes from them.
const RESERVE_RUNS: usize = 3;

/// A job's type and source, so runs compare whichever binary or output path they used:
/// the verb, `--to` and the first data file's name (by `[total] data_ext`). Without a data file, the whole command.
fn job_key(cmd: &str, data_ext: &[String]) -> String {
    let words: Vec<&str> = cmd.split_whitespace().collect();
    // A cargo job's memory follows its verb, crates and profile, not its test filter or -j.
    if let Some(at) = words.iter().position(|w| *w == "cargo") {
        let args = &words[at..];
        let crates = args.windows(2).filter(|w| w[0] == "-p").map(|w| w[1]);
        let profile = args.windows(2).find(|w| w[0] == "--profile").map(|w| w[1]).or(args.contains(&"--release").then_some("release"));
        return format!("cargo {} {} {}", args.get(1).unwrap_or(&""), crates.collect::<Vec<_>>().join(","), profile.unwrap_or("dev"));
    }
    let is_data = |w: &&str| Path::new(w).extension().is_some_and(|x| data_ext.iter().any(|m| x.eq_ignore_ascii_case(m)));
    let Some(at) = words.iter().position(is_data) else { return cmd.to_string() };
    let verb = words[..at].iter().rev().find(|w| !w.contains(['/', '=']) && !w.starts_with('-')).unwrap_or(&"");
    let to = words.windows(2).find(|w| w[0] == "--to").map_or("", |w| w[1]);
    let source = Path::new(words[at]).file_name().map_or(words[at].into(), |n| n.to_string_lossy());
    format!("{verb} {to} {source}")
}

/// The largest peak if all of the last runs stayed under half of `cap_mb`.
fn asks_too_much(cap_mb: u64, peaks: &[u64]) -> Option<u64> {
    let last = peaks.get(peaks.len().checked_sub(HINT_RUNS)?..)?;
    last.iter().copied().max().filter(|p| p * 2 < cap_mb)
}

/// What a job holds against the limits: 1.5 x its largest peak of the last runs, once there are enough, never above
/// its cap, which still kills it. `cargo test` peaking at 1 GB stood 15 min behind a 6G cap, 13.2 GB free, CPU 8 of 32.
/// shortcut: a key ignores --opt, so a heavier variant overshoots up to its cap; min_available is the margin for that.
fn reserve_mb(cap_mb: u64, peaks: &[u64]) -> u64 {
    let last = &peaks[peaks.len().saturating_sub(HINT_RUNS)..];
    match last.iter().max() {
        Some(max) if last.len() >= RESERVE_RUNS => cap_mb.min(max * 3 / 2 + 512),
        _ => cap_mb,
    }
}

/// When, which pid and what peak, MB, from a scope's line in `journalctl -o short-unix`.
fn scope_peak(line: &str) -> Option<(u64, i32, u64)> {
    let (ts, _) = line.split_once('.')?;
    let pid = line.split("run-p").nth(1)?.split('-').next()?.parse().ok()?;
    let peak = line.split(", ").find_map(|s| s.trim_end().trim_end_matches('.').strip_suffix(" memory peak"))?;
    Some((ts.parse().ok()?, pid, parse_mem(peak)?))
}

/// Peaks of finished jobs by `job_key`, oldest first. The journal has the peak and the pid; the
/// starts file the cap and command. Only new journal lines are read, and kept in `queue-peaks.txt`.
/// shortcut: both files grow without pruning, about 1 MB a day; prune when status slows down.
fn past_runs(dir: &Path, for_job: bool, data_ext: &[String]) -> HashMap<String, Vec<u64>> {
    let peaks_path = dir.join("queue-peaks.txt");
    let cursor = dir.join("queue-journal.cursor");
    // Every job comes through here. Without a cursor journalctl reads the whole journal, seconds: `status` pays that once, a job does not.
    if !for_job || cursor.exists() {
        let new = Command::new("journalctl")
            .args(["--user", "-o", "short-unix", "--no-pager", "-q", "-g", "scope: Consumed", "--cursor-file"])
            .arg(&cursor)
            .output();
        if let (Ok(new), Ok(mut f)) = (new, std::fs::File::options().create(true).append(true).open(&peaks_path)) {
            for (ts, pid, mb) in String::from_utf8_lossy(&new.stdout).lines().filter_map(scope_peak) {
                let _ = writeln!(f, "{ts} {pid} {mb}");
            }
        }
    }
    let mut by_pid: HashMap<i32, Vec<(u64, u64)>> = HashMap::new();
    for line in std::fs::read_to_string(&peaks_path).unwrap_or_default().lines() {
        let mut w = line.split(' ').filter_map(|v| v.parse::<u64>().ok());
        if let (Some(ts), Some(pid), Some(mb)) = (w.next(), w.next(), w.next()) {
            by_pid.entry(pid as i32).or_default().push((ts, mb));
        }
    }
    let mut runs: HashMap<String, Vec<u64>> = HashMap::new();
    for line in std::fs::read_to_string(dir.join(STARTS)).unwrap_or_default().lines() {
        let Ok(e) = serde_json::from_str::<Entry>(line) else { continue };
        // Its own ending, not an earlier job's under the same pid.
        if let Some((_, mb)) = by_pid.get(&e.pid).and_then(|p| p.iter().find(|(ts, _)| *ts >= e.since)) {
            runs.entry(job_key(&e.cmd, data_ext)).or_default().push(*mb);
        }
    }
    runs
}

pub fn status(config: &Path) -> Result<(), String> {
    let dir = config.parent().unwrap_or(Path::new("."));
    let st = State::open(dir)?;
    st.save()?;
    let mut entries = st.entries.clone();
    let now = crate::now();
    let avail_mb = (crate::mem_available_gb() * 1024.0) as u64;
    let anon_mb = running_anon_mb(&entries);
    let room_mb = note_room(&dir.join(ROOM), avail_mb + anon_mb, now);
    let disk_mb = disk_free_mb(dir);
    // Not under the queue's lock: the first read of the journal takes seconds.
    drop(st);
    let limits = std::fs::read_to_string(config).ok().and_then(|t| Limits::parse(&t).ok());
    let data_ext = limits.as_ref().map_or(&[][..], |l| &l.data_ext[..]);
    let runs = past_runs(dir, false, data_ext);
    println!("{:<5} {:<8} {:<6} {:<18} {:>7} {:>6} {:>6} {:>4} {:>6}  cmd", "job", "state", "kind", "user", "mem", "anon", "peak", "thr", "for s");
    entries.sort_by_key(|e| (!e.running, e.id));
    for e in &entries {
        let gb = |mb: u64| format!("{:.1}G", mb as f64 / 1024.0);
        let (anon, peak) = job_memory_mb(e.pid).filter(|_| e.running).map_or(("-".to_string(), "-".to_string()), |(a, p)| (gb(a), gb(p)));
        println!(
            "{:<5} {:<8} {:<6} {:<18} {:>6}G {anon:>6} {peak:>6} {:>4} {:>6}  {}",
            e.id,
            if e.running { "running" } else { "waiting" },
            e.kind,
            e.user,
            e.mem_mb / 1024,
            e.threads,
            now.saturating_sub(e.since),
            e.cmd.chars().take(80).collect::<String>()
        );
        if !e.running
            && let Some(l) = &limits
            && let Err(why) = disk_stop(l, disk_mb).and_then(|()| decide(&entries, e.id, l, avail_mb, anon_mb, room_mb, now))
        {
            println!("      waits for: {why}");
        }
        if let Some(peak) = runs.get(&job_key(&e.cmd, data_ext)).and_then(|r| asks_too_much(e.mem_mb, r)) {
            println!("      hint: cap {}G, peak {:.1}G in its last {HINT_RUNS} runs", e.mem_mb / 1024, peak as f64 / 1024.0);
        }
    }
    if entries.is_empty() {
        println!("(empty)");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no record of the room yet, as for a job that has not waited long.
    fn decide(entries: &[Entry], me: u64, limits: &Limits, avail_mb: u64, anon_mb: u64, now: u64) -> Result<(), String> {
        super::decide(entries, me, limits, avail_mb, anon_mb, None, now)
    }

    const CONF: &str = "
[total]
jobs = 3
memory = 20G
min_available = 1G
min_disk = 20G
[build]
jobs = 2
job_memory = 10G
job_threads = 2
max_runtime = 2h
cpu_weight = 200
[gate]
keep_binaries = 5
[heavy]
jobs = 2
job_memory = 8G
[user]
jobs = 2
[user.ci]
jobs = 1
";

    fn e(id: u64, kind: &str, user: &str, mem_g: u64, running: bool) -> Entry {
        Entry {
            id,
            pid: 0,
            lock: PathBuf::new(),
            user: user.into(),
            kind: kind.into(),
            mem_mb: mem_g * 1024,
            threads: 1,
            running,
            since: 0,
            cmd: String::new(),
        }
    }

    #[test]
    fn disk_stops_before_memory_is_asked() {
        let l = Limits::parse("[total]\nmin_disk = 20G\n").unwrap();
        assert_eq!(disk_stop(&l, Some(18 * 1024)), Err("18 GB of disk free, below min_disk 20 GB".into()));
        assert_eq!(disk_stop(&l, Some(20 * 1024)), Ok(()));
        // statvfs failing is not a full disk.
        assert_eq!(disk_stop(&l, None), Ok(()));
    }

    #[test]
    fn config() {
        let l = Limits::parse(CONF).unwrap();
        assert_eq!(l.total, Limit { jobs: Some(3), mem_mb: Some(20480), threads: None });
        assert_eq!(l.kinds["build"].job_mem_mb, Some(10240));
        assert_eq!((l.kinds["build"].max_runtime_s, l.kinds["build"].cpu_weight), (Some(7200), Some(200)));
        assert_eq!((l.min_disk_mb, l.keep_binaries), (20480, Some(5)));
        let ahead = Limits::parse("[gate]\nkeep = 1\n[heavy]\nsoon = 3\njobs = 2\n").unwrap();
        assert_eq!((ahead.unknown.len(), ahead.kinds["heavy"].limit.jobs), (2, Some(2)));
        assert!(Limits::parse("[heavy]\nmax_runtime = soon\n").is_err());
        assert_eq!((parse_secs("90"), parse_secs("30m"), parse_secs("1.5h")), (Some(90), Some(1800), Some(5400)));
        assert_eq!(l.user_limit("ci-runner").jobs, Some(1));
        assert_eq!(l.user_limit("alice").jobs, Some(2));
        assert!(Limits::parse("[total]\nmemory = lots\n").is_err());
        assert!(Limits::parse("jobs = 1\n").is_err());
        assert_eq!(parse_mem("1.5G"), Some(1536));
    }

    /// Memory enough for anything, and every test job long past its ramp.
    const BIG: u64 = 1 << 30;
    const OLD: u64 = 1 << 20;

    #[test]
    fn memory_is_what_the_machine_has_not_what_jobs_reserved() {
        let l = Limits::parse("[total]\nmemory = 20G\nmin_available = 8G\n[heavy]\njobs = 9\n[build]\njobs = 9\n[user]\njobs = 9\n").unwrap();
        const G: u64 = 1024;
        // Caps sum past [total] memory, the machine has room: it starts.
        let q = vec![e(1, "build", "a", 10, true), e(2, "heavy", "b", 12, false)];
        assert_eq!(decide(&q, 2, &l, 20 * G, 0, OLD), Ok(()));
        // Reaching its cap would leave less than the floor: it waits. Job 1 gives back the 1 GB it lacks, so it holds a 5G job behind it; a job of at most SMALL_JOB_MB passes.
        let q = vec![e(1, "build", "a", 1, true), e(2, "heavy", "b", 12, false), e(3, "heavy", "c", 5, false)];
        assert!(decide(&q, 2, &l, 19 * G, 0, OLD).unwrap_err().contains("needs its 12288 MB"));
        assert!(decide(&q, 3, &l, 19 * G, G, OLD).unwrap_err().starts_with("behind job 2"));
        let q = vec![e(1, "build", "a", 1, true), e(2, "heavy", "b", 12, false), e(3, "heavy", "c", 4, false)];
        assert_eq!(decide(&q, 3, &l, 19 * G, G, OLD), Ok(()));
        // A job that has just started has not grown yet: its cap counts until the ramp is over.
        let q = vec![e(1, "build", "a", 10, true), e(2, "heavy", "b", 4, false)];
        assert!(decide(&q, 2, &l, 20 * G, 0, RAMP_S - 1).is_err());
        assert_eq!(decide(&q, 2, &l, 20 * G, 0, RAMP_S), Ok(()));
        // Waiters ahead that fit take their cap out of what is left, so two cannot start on the same room.
        let q = vec![e(1, "heavy", "a", 8, false), e(2, "heavy", "b", 8, false)];
        assert_eq!(decide(&q, 1, &l, 20 * G, 0, OLD), Ok(()));
        assert!(decide(&q, 2, &l, 20 * G, 0, OLD).is_err());
        // Three jobs have barely grown and the machine looks empty: the fourth still waits, they may all peak.
        let ceiling = Limits::parse("[total]\nmin_available = 8G\nmax_caps = 44G\n[heavy]\njobs = 9\n[user]\njobs = 9\n").unwrap();
        let q = vec![e(1, "heavy", "a", 14, true), e(2, "heavy", "b", 14, true), e(3, "heavy", "c", 14, true), e(4, "heavy", "d", 3, false), e(5, "heavy", "f", 2, false)];
        assert!(decide(&q, 4, &ceiling, BIG, 0, OLD).unwrap_err().contains("max_caps"));
        assert_eq!(decide(&q, 5, &ceiling, BIG, 0, OLD), Ok(()), "a job of at most SMALL_JOB_MB passes the holder, however long it has held");
        let q = vec![e(1, "heavy", "a", 14, true), e(2, "heavy", "b", 14, true), e(3, "heavy", "c", 14, true), e(4, "heavy", "d", 2, false)];
        assert_eq!(decide(&q, 4, &ceiling, BIG, 0, OLD), Ok(()));
        // A lane needs only the floor, and nobody waits on memory behind a job of the lane.
        let lane = Limits::parse("[total]\nmin_available = 8G\n[light]\noutside_total = yes\n[user]\njobs = 9\n").unwrap();
        let q = vec![e(1, "heavy", "a", 12, false), e(2, "light", "b", 1, false)];
        assert!(decide(&q, 1, &lane, 9 * G, 0, OLD).is_err());
        assert_eq!(decide(&q, 2, &lane, 9 * G, 0, OLD), Ok(()));
        assert!(decide(&q, 2, &lane, 7 * G, 0, OLD).is_err());
    }

    /// The snapshot that held everything: 3 jobs, caps 28G, 24 GB free, jobs of 5-12G waiting.
    #[test]
    fn a_cargo_job_is_keyed_by_verb_crates_and_profile() {
        let k = |c: &str| job_key(c, &[]);
        assert_eq!(k("cargo test -p io step::"), k("timeout 900 cargo test -j 2 -p io --lib other::"));
        assert_ne!(k("cargo test -p io"), k("cargo test -p core"));
        assert_ne!(k("cargo build -p a"), k("cargo build --release -p a"));
    }

    #[test]
    fn a_job_reserves_from_its_peaks_once_there_are_three_and_never_above_its_cap() {
        assert_eq!(reserve_mb(6144, &[1000, 1000]), 6144);
        assert_eq!(reserve_mb(6144, &[1000, 800, 600]), 2012);
        assert_eq!(reserve_mb(6144, &[9000, 1000, 1000, 1000, 1000, 1000]), 2012);
        assert_eq!(reserve_mb(6144, &[5000, 5000, 5000]), 6144);
    }

    #[test]
    fn a_hint_needs_five_runs_of_one_type_and_source_all_under_half_the_cap() {
        let ext = ["dat".to_string(), "csv".to_string()];
        let k = |c: &str| job_key(c, &ext);
        let a = k("/usr/bin/time -f x ./app.base convert ../../a/big.dat --to json -o /tmp/o1.json");
        assert_eq!(a, "convert json big.dat");
        assert_eq!(a, k("env SOURCE_DATE_EPOCH=1 /x/bin/new convert /y/big.dat --to json -o b.json"));
        assert_ne!(a, k("./i convert big.dat --to csv"));
        assert_eq!(k("./i stats --fast ../x/rows.csv"), "stats  rows.csv");
        assert_eq!(k("cargo test -p app"), "cargo test app dev");
        assert_eq!(job_key("./i stats rows.csv", &[]), "./i stats rows.csv", "no data_ext: the whole command");
        assert_eq!(Limits::parse("[total]\ndata_ext = DAT csv\n").unwrap().data_ext, ["dat", "csv"]);
        let g = 1024;
        assert_eq!(asks_too_much(12 * g, &[g, g, g, g]), None, "four runs say nothing");
        assert_eq!(asks_too_much(12 * g, &[9 * g, g, 2 * g, g, g, 3 * g]), Some(3 * g), "only the last five count");
        assert_eq!(asks_too_much(12 * g, &[g, g, g, g, 6 * g]), None, "one at half: the cap was needed");
        assert_eq!(
            scope_peak("1793552915.689218 host systemd[11276]: run-p2059841-i253430981.scope: Consumed 3.498s CPU time over 3.504s wall clock time, 82.2M memory peak."),
            Some((1793552915, 2059841, 82))
        );
        assert_eq!(
            scope_peak("1793552915.1 host systemd[1]: run-p7-i9.scope: Consumed 1s CPU time, 4.5G memory peak, 1G memory swap peak."),
            Some((1793552915, 7, 4608))
        );
        assert_eq!(scope_peak("1793552915.1 host systemd[1]: run-p7-i9.scope: Consumed 1s CPU time."), None);
    }

    #[test]
    fn the_oldest_job_holds_past_twenty_minutes_and_only_for_room_that_will_come() {
        // A real snapshot: 1339 waited 1 h 44 min on 32.5 GB against the 32 it needs,
        // flickering in and out of holding, while 1724 stood five jobs behind it after 5 minutes.
        let l = Limits::parse("[total]\njobs = 8\nthreads = 96\nmin_available = 8G\nmax_caps = 44G\n[heavy]\njobs = 5\n[build]\njobs = 3\n[user]\njobs = 9\n").unwrap();
        const NOW: u64 = 1 << 20;
        const AVAIL: u64 = 20194;
        const ANON: u64 = 12339;
        let job = |id: u64, kind: &str, mem_g: u64, running: bool, age: u64| Entry { since: NOW - age, ..e(id, kind, &format!("u{id}"), mem_g, running) };
        let running = [job(1712, "heavy", 18, true, 658), job(1743, "heavy", 6, true, 241)];
        let waiting = [job(1339, "heavy", 24, false, 6308), job(1724, "heavy", 12, false, 671), job(1744, "heavy", 6, false, 242), job(1745, "build", 8, false, 170)];
        let q: Vec<Entry> = running.iter().chain(&waiting).cloned().collect();
        // 32 533 MB now, 235 short of the 32 768 it needs, and it was there in the window: it holds them all.
        const SEEN: Option<u64> = Some(33 * 1024);
        assert!(super::decide(&q, 1339, &l, AVAIL, ANON, SEEN, NOW).unwrap_err().contains("max_caps"));
        for id in [1724, 1744, 1745] {
            assert!(super::decide(&q, id, &l, AVAIL, ANON, SEEN, NOW).unwrap_err().starts_with("behind job 1339"), "{id}");
        }
        // Without it, 11 minutes hold nobody: the 12G that does not fit waits, the 6G and the build start.
        let q: Vec<Entry> = running.iter().chain(&waiting[1..]).cloned().collect();
        assert!(decide(&q, 1724, &l, AVAIL, ANON, NOW).unwrap_err().contains("needs its 12288 MB"));
        assert_eq!(decide(&q, 1744, &l, AVAIL, ANON, NOW), Ok(()));
        // Past 40 minutes no job above SMALL_JOB_MB passes it, though the room is there; small ones and the light lane do.
        let lane = Limits::parse("[total]\njobs = 8\nmin_available = 8G\nmax_caps = 44G\n[heavy]\njobs = 5\n[build]\njobs = 3\n[light]\noutside_total = yes\n[user]\njobs = 9\n").unwrap();
        let big = |age| job(2032, "heavy", 14, false, age);
        let q = |age| vec![job(1712, "heavy", 18, true, 5000), big(age), job(2189, "heavy", 8, false, 8), job(2190, "build", 1, false, 8), job(2191, "light", 1, false, 8)];
        const ROOM: u64 = 20 * 1024;
        assert_eq!(decide(&q(STRICT_AFTER_S - 1), 2189, &lane, ROOM, 10 * 1024, NOW), Ok(()), "under 40 minutes it passes: 14 + 8 + 8 fit in what comes");
        assert!(decide(&q(STRICT_AFTER_S), 2189, &lane, ROOM, 10 * 1024, NOW).unwrap_err().starts_with("behind job 2032"));
        assert_eq!(decide(&q(STRICT_AFTER_S), 2190, &lane, ROOM, 10 * 1024, NOW), Ok(()), "a job of at most SMALL_JOB_MB passes");
        assert_eq!(decide(&q(STRICT_AFTER_S), 2191, &lane, ROOM, 10 * 1024, NOW), Ok(()), "the light lane never waits");
        // Only the oldest holds: at 20 minutes the second one is not a second reservation.
        let old = [job(1, "heavy", 24, false, 6000), job(2, "heavy", 24, false, 5000)];
        let q: Vec<Entry> = running.iter().chain(&old).cloned().collect();
        assert!(super::decide(&q, 2, &l, AVAIL, ANON, SEEN, NOW).unwrap_err().starts_with("behind job 1"));
        // Nothing runs and it still does not fit: nothing will come, so the machine works meanwhile.
        let q = [job(1339, "heavy", 24, false, 6308), job(1744, "heavy", 6, false, 242)];
        assert!(decide(&q, 1339, &l, 31 * 1024, 0, NOW).is_err());
        assert_eq!(decide(&q, 1744, &l, 31 * 1024, 0, NOW), Ok(()));
    }

    #[test]
    fn room_not_seen_in_ten_minutes_holds_nobody_and_says_so() {
        // A real snapshot: 2223 (24G) waited 5456 s holding four jobs, one build of 0.1 GB running,
        // MemAvailable 23.7-31.8 GB against the 32 it needs. 2 GB of slack called that room to come.
        let l = Limits::parse("[total]\njobs = 8\nmin_available = 8G\nmax_caps = 44G\n[heavy]\njobs = 5\n[build]\njobs = 3\n[user]\njobs = 9\n").unwrap();
        const NOW: u64 = 1 << 20;
        let job = |id: u64, kind: &str, mem_g: u64, running: bool, age: u64| Entry { since: NOW - age, ..e(id, kind, &format!("u{id}"), mem_g, running) };
        let q = [job(2300, "build", 8, true, 60), job(2223, "heavy", 24, false, 5456), job(2301, "heavy", 6, false, 300), job(2302, "build", 8, false, 200)];
        const AVAIL: u64 = 28 * 1024;
        const ANON: u64 = 102;
        const PEAK: u64 = 32665;
        let why = super::decide(&q, 2223, &l, AVAIL, ANON, Some(PEAK), NOW).unwrap_err();
        assert_eq!(why, "will not get in: needs 32768 MB, at most 32665 MB in the last 10 min: lower its -m");
        assert_eq!(super::decide(&q, 2301, &l, AVAIL, ANON, Some(PEAK), NOW), Ok(()));
        assert_eq!(super::decide(&q, 2302, &l, AVAIL, ANON, Some(PEAK), NOW), Ok(()));
        // Reached once in the window, it holds though it is short now: that room comes and goes.
        assert!(super::decide(&q, 2301, &l, AVAIL, ANON, Some(32768), NOW).unwrap_err().starts_with("behind job 2223"));
        // A lane needs only the floor: the record says nothing about it.
        let lane = Limits::parse("[total]\nmin_available = 8G\n[light]\noutside_total = yes\n[user]\njobs = 9\n").unwrap();
        let q = [job(1, "light", 1, false, 5)];
        assert_eq!(super::decide(&q, 1, &lane, 9 * 1024, 0, Some(100), NOW), Ok(()));
    }

    #[test]
    fn pages_given_up_with_madv_free_are_not_held() {
        // A real job: 6.4 GB `anon`, 6.3 of it freed lazily, on the file lists.
        let stat = "anon 6710886400\nfile 13631488\ninactive_anon 104857600\nactive_anon 209715200\ninactive_file 6606028800\nactive_file 13631488\n";
        assert_eq!(held_anon(stat), Some(314572800));
        assert_eq!(held_anon("anon 5\n"), None);
    }

    #[test]
    fn the_record_of_room_answers_only_for_a_whole_window() {
        let path = std::env::temp_dir().join(format!("regate-room-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&path);
        const T: u64 = 1 << 20;
        assert_eq!(note_room(&path, 500, T), None, "nothing on record yet");
        let mut last = None;
        for t in (10..=ROOM_WINDOW_S).step_by(10) {
            last = note_room(&path, 100, T + t);
        }
        assert_eq!(last, Some(500), "ten minutes on record, its peak at the far end");
        assert_eq!(note_room(&path, 200, T + ROOM_WINDOW_S + 5), Some(500), "within a step: one line, the most of both");
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().last(), Some(&*format!("{} 200", T + ROOM_WINDOW_S)));
        assert_eq!(note_room(&path, 100, T + ROOM_WINDOW_S + 30), Some(200), "the peak aged out");
        assert_eq!(note_room(&path, 100, T + ROOM_WINDOW_S + 300), None, "a hole in the record may hide a peak");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_big_job_holds_only_what_would_delay_it() {
        // A real snapshot: one 8G job holding 280 MB, 23.5 GB free, one slot of eight
        // working, everything behind an 18G job that needs 26 GB.
        let l = Limits::parse("[total]\njobs = 9\nmin_available = 8G\nmax_caps = 44G\n[heavy]\njobs = 9\n[user]\njobs = 9\n").unwrap();
        const NOW: u64 = 1 << 20;
        let job = |id: u64, mem_g: u64, running: bool, age: u64| Entry { since: NOW - age, ..e(id, "heavy", &format!("u{id}"), mem_g, running) };
        let q = [job(1, 8, true, 1400), job(2, 18, false, HOLD_AFTER_S + 100), job(3, 8, false, 30)];
        const AVAIL: u64 = 24064;
        assert!(decide(&q, 2, &l, AVAIL, 280, NOW).unwrap_err().contains("needs its 18432 MB"));
        assert_eq!(decide(&q, 3, &l, AVAIL, 280, NOW), Ok(()), "26 GB will not come when job 1 ends: nothing to hold for");
        // Job 1 really holds 3 GB: when it ends there is room, so the 8G job would stand in the way.
        assert!(decide(&q, 3, &l, AVAIL, 3 * 1024, NOW).unwrap_err().starts_with("behind job 2"));
        // 8 GB to spare past the big job: this one does not delay it, whenever it ends.
        assert_eq!(decide(&q, 3, &l, AVAIL, 10 * 1024 + 512, NOW), Ok(()));
        // Started past it, it takes that spare room; the next one waits, or a stream would starve the big one.
        let q = [job(1, 8, true, 1400), job(3, 8, true, 10), job(2, 18, false, HOLD_AFTER_S + 100), job(4, 8, false, 5)];
        assert!(decide(&q, 4, &l, BIG, 0, NOW).is_ok(), "the machine has room for all");
        assert!(decide(&q, 4, &l, AVAIL, 10 * 1024 + 512, NOW).unwrap_err().starts_with("behind job 2"));
    }

    #[test]
    fn small_jobs_start_on_real_room_past_a_big_one() {
        let l = Limits::parse("[total]\njobs = 9\nmin_available = 8G\nmax_caps = 44G\n[heavy]\njobs = 9\n[user]\njobs = 9\n").unwrap();
        const NOW: u64 = 1 << 20;
        let job = |id: u64, mem_g: u64, running: bool, age: u64| Entry { since: NOW - age, ..e(id, "heavy", &format!("u{id}"), mem_g, running) };
        let running = [job(1, 12, true, 100), job(2, 12, true, 100), job(3, 4, true, 100)];
        const AVAIL: u64 = 24 * 1024;
        // A job that cannot fit (20G + the floor) waits; both 5G jobs behind it go, and the caps stay under 44G.
        let q: Vec<Entry> = running.iter().cloned().chain([job(4, 20, false, 60), job(5, 5, false, 30), job(6, 5, false, 30)]).collect();
        assert!(decide(&q, 4, &l, AVAIL, 0, NOW).is_err());
        assert_eq!(decide(&q, 5, &l, AVAIL, 0, NOW), Ok(()));
        assert_eq!(decide(&q, 6, &l, AVAIL, 0, NOW), Ok(()));
        // The room is not given twice: the third 5G fits (24 - 10 >= 5 + 8), not with 2 GB less.
        let q: Vec<Entry> = running.iter().cloned().chain([job(5, 5, false, 30), job(6, 5, false, 30), job(7, 5, false, 30), job(8, 5, false, 30)]).collect();
        assert_eq!(decide(&q, 7, &l, AVAIL, 0, NOW), Ok(()));
        assert!(decide(&q, 7, &l, AVAIL - 2048, 0, NOW).unwrap_err().contains("needs its 5120 MB"));
        // The fourth would have memory on a bigger machine, but 28 + 20 passes the ceiling.
        assert!(decide(&q, 8, &l, BIG, 0, NOW).unwrap_err().contains("max_caps"));
        // Room in memory, none in the caps: 28 + 12 + 5 passes 44.
        let q: Vec<Entry> = running.iter().cloned().chain([job(4, 12, false, 30), job(5, 5, false, 30)]).collect();
        assert_eq!(decide(&q, 4, &l, BIG, 0, NOW), Ok(()));
        assert!(decide(&q, 5, &l, BIG, 0, NOW).unwrap_err().contains("max_caps"));
        // Past HOLD_AFTER_S the big job holds those that would delay it, or a stream of small ones starves it.
        let q: Vec<Entry> = running.iter().cloned().chain([job(4, 20, false, HOLD_AFTER_S), job(5, 5, false, 30)]).collect();
        assert!(decide(&q, 5, &l, AVAIL, 4 * 1024, NOW).unwrap_err().starts_with("behind job 4"));
        // The limits that really held the snapshot say which, and with what numbers.
        let threads = Limits::parse("[total]\nthreads = 48\n[heavy]\njobs = 9\n[user]\njobs = 9\n").unwrap();
        let q = [Entry { threads: 40, ..job(1, 12, true, 100) }, Entry { threads: 16, ..job(2, 5, false, 30) }];
        assert_eq!(decide(&q, 2, &threads, AVAIL, 0, NOW).unwrap_err(), "machine total limit: threads 40+16 of 48");
    }

    #[test]
    fn a_number_moving_is_not_a_new_reason() {
        assert!(same_reason("16558 MB free, needs its 3200 MB cap", "16243 MB free, needs its 3200 MB cap"));
        assert!(!same_reason("16558 MB free, needs its 3200 MB cap", "[heavy] limit"));
    }

    #[test]
    fn admission() {
        let l = Limits::parse(CONF).unwrap();
        // Two builds run: a third build waits on [build], a heavy one behind it still starts.
        let q = vec![e(1, "build", "a", 4, true), e(2, "build", "b", 4, true), e(3, "build", "c", 4, false), e(4, "heavy", "c", 4, false)];
        assert_eq!(decide(&q, 3, &l, BIG, 0, OLD), Err("[build] limit".into()));
        assert_eq!(decide(&q, 4, &l, BIG, 0, OLD), Ok(()));
        // The machine's job total holds a heavy job, and everyone behind it by the same limit.
        let q = vec![e(1, "build", "a", 1, true), e(2, "build", "b", 1, true), e(5, "heavy", "z", 1, true), e(6, "heavy", "y", 1, false), e(7, "heavy", "x", 1, false)];
        assert!(decide(&q, 6, &l, BIG, 0, OLD).unwrap_err().starts_with("machine total limit"));
        assert!(decide(&q, 7, &l, BIG, 0, OLD).unwrap_err().starts_with("machine total limit"));
        // A user at its own limit is passed over.
        let q = vec![e(1, "heavy", "ci", 1, true), e(2, "heavy", "ci", 1, false), e(3, "heavy", "z", 1, false)];
        assert_eq!(decide(&q, 2, &l, BIG, 0, OLD), Err("user ci limit".into()));
        assert_eq!(decide(&q, 3, &l, BIG, 0, OLD), Ok(()));
        // A lane outside the total: it passes a machine that is full and a job waiting for it, and takes nothing from it.
        let lane = Limits::parse("[total]\njobs = 1\n[heavy]\njobs = 2\n[light]\njobs = 1\noutside_total = yes\n").unwrap();
        let q = vec![e(1, "heavy", "a", 1, true), e(2, "heavy", "b", 1, false), e(3, "light", "c", 1, false), e(4, "light", "d", 1, false)];
        assert!(decide(&q, 2, &lane, BIG, 0, OLD).is_err());
        assert_eq!(decide(&q, 3, &lane, BIG, 0, OLD), Ok(()));
        assert!(decide(&q, 4, &lane, BIG, 0, OLD).unwrap_err().contains("[light]"), "its own limit still holds");
        let q = vec![e(1, "light", "a", 1, true), e(2, "heavy", "b", 1, false)];
        assert_eq!(decide(&q, 2, &lane, BIG, 0, OLD), Ok(()));
        // A job held by its kind holds nobody: a build goes past the third heavy job.
        let q = vec![e(1, "heavy", "a", 8, true), e(2, "heavy", "b", 8, true), e(3, "heavy", "c", 8, false), e(4, "build", "d", 2, false)];
        assert_eq!(decide(&q, 3, &l, BIG, 0, OLD), Err("[heavy] limit".into()));
        assert_eq!(decide(&q, 4, &l, BIG, 0, OLD), Ok(()));
        // Earlier waiters that fit are counted as started: the third job overall waits.
        let q = vec![e(1, "heavy", "a", 1, false), e(2, "build", "b", 1, false), e(3, "build", "c", 1, false), e(4, "heavy", "d", 1, false)];
        assert_eq!(decide(&q, 3, &l, BIG, 0, OLD), Ok(()));
        assert!(decide(&q, 4, &l, BIG, 0, OLD).unwrap_err().starts_with("machine total limit"));
    }
}
