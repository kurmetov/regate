//! Load test of `regate queue`: many clients at once, every limit checked over the
//! whole run from the jobs' own start and end stamps; a killed job frees its place; a real
//! scope gets the caps the config gives.
//!
//! Stamps are `date +%s%N`, never `%3N`: uutils date 0.10 drops the leading zeros of `%3N`
//! (nanoseconds 010618311 print as 106), which shifts a stamp by up to 0.9 s.

use std::path::{Path, PathBuf};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const GATE: &str = env!("CARGO_BIN_EXE_regate");

fn dir(name: &str, config: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("gate-load-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("limits.conf"), config).unwrap();
    d
}

fn queue(d: &Path, kind: &str, user: &str, mem: &str, scope: bool, script: &str) -> Child {
    let mut c = Command::new(GATE);
    c.arg("queue").arg("--config").arg(d.join("limits.conf")).args(["--kind", kind, "--user", user, "-m", mem]);
    if !scope {
        c.arg("--no-scope");
    }
    // Its own process group, as the gate runs it, so a kill can reach the whole job.
    c.args(["--", "sh", "-c", script]).process_group(0).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap()
}

fn now_ns() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

#[derive(Clone, Copy)]
struct Spec {
    kind: &'static str,
    user: &'static str,
    mem_mb: u64,
}

#[test]
fn forty_clients_never_exceed_a_limit_and_all_finish() {
    let d = dir(
        "forty",
        "[total]\njobs = 4\nmin_available = 0\n\
         [build]\njobs = 2\njob_memory = 100M\n[heavy]\njobs = 3\njob_memory = 200M\n\
         [user]\njobs = 2\n[user.slow]\njobs = 1\n",
    );
    let users = ["a", "b", "c", "slow"];
    let mut specs: Vec<Spec> = (0..40)
        .map(|i| Spec {
            kind: if i % 3 == 0 { "build" } else { "heavy" },
            user: users[i % 4],
            mem_mb: if i % 3 == 0 { 100 } else { 200 },
        })
        .collect();
    specs[5] = Spec { kind: "heavy", user: "big", mem_mb: 800 }; // must not starve
    let log = d.join("stamps");
    let started = Instant::now();
    let mut children = vec![];
    for (i, s) in specs.iter().enumerate() {
        let script = format!(
            "echo \"start $(date +%s%N) {i}\" >> {log}; sleep 0.3; echo \"end $(date +%s%N) {i}\" >> {log}",
            log = log.display()
        );
        children.push(queue(&d, s.kind, s.user, &format!("{}M", s.mem_mb), false, &script));
        std::thread::sleep(Duration::from_millis(5));
    }
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    let wall = started.elapsed();
    let text = std::fs::read_to_string(&log).unwrap();
    let mut span = vec![(0u128, 0u128); specs.len()];
    let mut events = vec![];
    for l in text.lines() {
        let w: Vec<&str> = l.split_whitespace().collect();
        let (t, i): (u128, usize) = (w[1].parse().unwrap(), w[2].parse().unwrap());
        if w[0] == "start" {
            span[i].0 = t;
            events.push((t, 1i64, i));
        } else {
            span[i].1 = t;
            events.push((t, -1i64, i));
        }
    }
    assert_eq!(events.len(), 80, "every job started and ended once");
    // Ends before starts at the same instant: a freed place is taken after it is freed.
    events.sort_by_key(|&(t, d, _)| (t, d));
    let (mut jobs, mut build, mut heavy) = (0i64, 0i64, 0i64);
    let mut per_user = std::collections::HashMap::<&str, i64>::new();
    let mut peak = 0;
    for (_, delta, i) in events {
        let s = specs[i];
        jobs += delta;
        *if s.kind == "build" { &mut build } else { &mut heavy } += delta;
        *per_user.entry(s.user).or_default() += delta;
        peak = peak.max(jobs);
        assert!(jobs <= 4 && build <= 2 && heavy <= 3, "jobs {jobs} build {build} heavy {heavy}");
        let cap = if s.user == "slow" { 1 } else { 2 };
        assert!(per_user[s.user] <= cap, "user {} at {}", s.user, per_user[s.user]);
    }
    assert_eq!(peak, 4, "the queue fills the machine, not just stays safe");
    // The 8G job queued sixth; at most a handful queued after it may start before it.
    let before_big = (6..specs.len()).filter(|&i| span[i].0 < span[5].0).count();
    assert!(before_big <= 4, "{before_big} later jobs overtook the big one");
    // 40 jobs of 0.3 s, at most 4 at once: 3 s of work; the rest is waiting on limits and polling.
    let first = span.iter().map(|s| s.0).min().unwrap();
    eprintln!("40 jobs in {:.1} s, the big one started at +{} ms", wall.as_secs_f64(), (span[5].0 - first) / 1_000_000);
    assert!(wall < Duration::from_secs(30), "took {wall:?}");
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn a_killed_job_frees_its_place_once_all_its_processes_are_gone() {
    let d = dir("kill", "[total]\njobs = 1\nmin_available = 0\n[heavy]\njob_memory = 1G\n");
    // A shell with a child: the place stays taken while either lives.
    let mut first = queue(&d, "heavy", "a", "1G", false, "sleep 60; true");
    let running = |d: &Path| {
        let out = Command::new(GATE).args(["queue", "status", "--config"]).arg(d.join("limits.conf")).output().unwrap();
        String::from_utf8_lossy(&out.stdout).matches("running").count()
    };
    let t = Instant::now();
    while running(&d) == 0 {
        assert!(t.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(50));
    }
    let stamp = d.join("second");
    let mut second = queue(&d, "heavy", "b", "1G", false, &format!("date +%s%N > {}", stamp.display()));
    std::thread::sleep(Duration::from_millis(800));
    assert!(!stamp.exists(), "the second job ran beside the first");
    let killed = now_ns();
    unsafe { libc::kill(-(first.id() as i32), libc::SIGKILL) };
    first.wait().unwrap();
    assert!(second.wait().unwrap().success());
    let at: u128 = std::fs::read_to_string(&stamp).unwrap().trim().parse().unwrap();
    assert!(at - killed < 2_000_000_000, "the place came free {} ms after the kill", (at - killed) / 1_000_000);
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn the_queue_marks_the_handover_for_the_gate_and_hides_it_from_the_job() {
    let d = dir("mark", "[total]\njobs = 1\n[heavy]\njob_memory = 1G\n");
    let mark = d.join("started");
    let mut holder = queue(&d, "heavy", "a", "1G", false, "sleep 1");
    std::thread::sleep(Duration::from_millis(300));
    let before = now_ns();
    let out = Command::new(GATE)
        .args(["queue", "--no-scope", "--config"])
        .arg(d.join("limits.conf"))
        .args(["--", "sh", "-c", "echo ${REGATE_QUEUE_STARTED:-hidden}"])
        .env("REGATE_QUEUE_STARTED", &mark)
        .output()
        .unwrap();
    holder.wait().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hidden");
    let text = std::fs::read_to_string(&mark).unwrap();
    let (ns, cpu) = text.split_once(' ').unwrap();
    assert!(ns.parse::<u128>().unwrap() > before + 300_000_000, "marked before the wait ended: {text}");
    assert!(cpu.parse::<f64>().unwrap() >= 0.0);
    std::fs::remove_dir_all(d).unwrap();
}

/// Reservations no longer hold a job back: two jobs whose caps sum past [total] memory run
/// together. A cap the machine cannot give waits, says why, and comes from the real MemAvailable.
#[test]
fn memory_comes_from_the_machine_not_from_the_sum_of_caps() {
    let d = dir("real-memory", "[total]\nmemory = 100M\nmin_available = 0\n[heavy]\njob_memory = 100M\n[user]\njobs = 9\n");
    let mut first = queue(&d, "heavy", "a", "100M", false, "sleep 2");
    let t = Instant::now();
    let mut second = queue(&d, "heavy", "b", "100M", false, "true");
    assert!(second.wait().unwrap().success());
    assert!(t.elapsed() < Duration::from_secs(1), "the second job waited for the first: {:?}", t.elapsed());
    first.wait().unwrap();
    // More than any machine: it waits and says what it lacks. [total] memory is left out, or it would be refused.
    std::fs::write(d.join("limits.conf"), "[total]\nmin_available = 0\n[heavy]\njob_memory = 1G\n").unwrap();
    let mut c = Command::new(GATE)
        .args(["queue", "--no-scope", "--config"])
        .arg(d.join("limits.conf"))
        .args(["-m", "99999G", "--", "true"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut said = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(c.stderr.take().unwrap()), &mut said).unwrap();
    c.kill().unwrap();
    c.wait().unwrap();
    assert!(said.contains("MB free, needs its"), "{said}");
    std::fs::remove_dir_all(d).unwrap();
}

/// A gate mid-run keeps working when its binary is replaced: `cargo install` renames over it,
/// and the running process's own path then reads "... (deleted)". All cases left in a run
/// used to end in "gate error: No such file or directory".
#[test]
fn a_run_survives_its_binary_being_replaced() {
    let d = dir("replaced", "[total]\nmin_available = 0\n[heavy]\njob_memory = 100M\n[user]\njobs = 9\n");
    let root = d.join("root");
    std::fs::create_dir_all(root.join(".regate")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::rename(d.join("limits.conf"), root.join(".regate/limits.conf")).unwrap();
    std::fs::write(root.join("tests/gate.cases"), "[s] mem=100M\nfirst a\nsecond b\n").unwrap();
    assert!(Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap().success());
    let stamp = d.join("first-ran");
    let fake = d.join("app");
    // The first case holds the run until the test has replaced the gate.
    std::fs::write(&fake, format!("#!/bin/sh\n[ \"$1\" = a ] && {{ touch {stamp}; sleep 1; }}\nexit 0\n", stamp = stamp.display())).unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let gate = d.join("gate-copy");
    std::fs::copy(GATE, &gate).unwrap();
    let run = Command::new(&gate)
        .args(["run", "s", "-j", "1", "--bin"])
        .arg(&fake)
        .current_dir(&root)
        .env("REGATE_QUEUE_USER", "t")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let t = Instant::now();
    while !stamp.exists() {
        assert!(t.elapsed() < Duration::from_secs(20), "the first case never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    // What cargo install does: a new file renamed over the old one.
    std::fs::copy(GATE, d.join("gate-new")).unwrap();
    std::fs::rename(d.join("gate-new"), &gate).unwrap();
    let out = run.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && !text.contains("gate error"), "{text}");
    assert_eq!(std::fs::read_to_string(root.join(".regate/.gitignore")).unwrap(), "*\n", "the data dir keeps itself out of git");
    std::fs::remove_dir_all(d).unwrap();
}

/// gate.conf `check` sees what a hash cannot: an output written fine but empty inside.
#[test]
fn a_failed_check_fails_the_run_and_is_no_baseline() {
    let d = dir("check", "[total]\nmin_available = 0\n[heavy]\njob_memory = 100M\n[user]\njobs = 9\n");
    let root = d.join("root");
    std::fs::create_dir_all(root.join(".regate")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::rename(d.join("limits.conf"), root.join(".regate/limits.conf")).unwrap();
    std::fs::write(root.join("tests/gate.cases"), "[s] mem=100M\nm.txt $out\n").unwrap();
    std::fs::write(root.join("gate.conf"), "check = sh check.sh\n").unwrap();
    std::fs::write(root.join("check.sh"), "grep -q body \"$@\" || { echo \"no body in $1\"; exit 1; }\n").unwrap();
    assert!(Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap().success());
    let run = |body: &str| {
        let fake = d.join("app");
        std::fs::write(&fake, format!("#!/bin/sh\nprintf '{body}\\n' > \"$1\"\n")).unwrap();
        std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        Command::new(GATE).args(["run", "s", "-j", "1", "--bin"]).arg(&fake).current_dir(&root).env("REGATE_QUEUE_USER", "t").output().unwrap()
    };
    let empty = run("head only");
    let text = String::from_utf8_lossy(&empty.stdout);
    assert!(!empty.status.success() && text.contains("ok but check failed"), "{text}");
    let full = run("head and body");
    let text = String::from_utf8_lossy(&full.stdout);
    assert!(full.status.success() && text.contains("first run"), "the failed run is no baseline: {text}");
    std::fs::remove_dir_all(d).unwrap();
}

/// One binary's cases share an output directory and log names: a second run removed the first's output,
/// and a good run read as `ok but wrote nothing`.
#[test]
fn two_runs_of_one_binary_take_a_case_in_turn() {
    let d = dir("turns", "[total]\nmin_available = 0\n[heavy]\njob_memory = 100M\n[user]\njobs = 9\n");
    let root = d.join("root");
    std::fs::create_dir_all(root.join(".regate")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::rename(d.join("limits.conf"), root.join(".regate/limits.conf")).unwrap();
    std::fs::write(root.join("tests/gate.cases"), "[s] mem=100M\nm.txt $out\n").unwrap();
    assert!(Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap().success());
    let (busy, overlap) = (d.join("busy"), d.join("overlap"));
    let fake = d.join("app");
    let script = format!("#!/bin/sh\nmkdir {b} || touch {o}\necho x > \"$1\"\nsleep 1\nrmdir {b}\n", b = busy.display(), o = overlap.display());
    std::fs::write(&fake, script).unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let start = || {
        Command::new(GATE).args(["run", "s", "-j", "1", "--fresh", "--bin"]).arg(&fake).current_dir(&root).env("REGATE_QUEUE_USER", "t").stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap()
    };
    let (a, b) = (start(), start());
    for run in [a, b] {
        let out = run.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    }
    assert!(!overlap.exists(), "both ran the case at once");
    std::fs::remove_dir_all(d).unwrap();
}

/// The key holds `$out` unexpanded: two cases whose arguments differ only in it got each other's cached result.
#[test]
fn cases_with_the_same_arguments_keep_their_own_results() {
    let d = dir("samekey", "[total]\nmin_available = 0\n[heavy]\njob_memory = 100M\n[user]\njobs = 9\n");
    let root = d.join("root");
    std::fs::create_dir_all(root.join(".regate")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::rename(d.join("limits.conf"), root.join(".regate/limits.conf")).unwrap();
    std::fs::write(root.join("tests/gate.cases"), "[s] mem=100M\nr.xlsx $out\nr.pdf $out\n").unwrap();
    assert!(Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap().success());
    let fake = d.join("app");
    std::fs::write(&fake, "#!/bin/sh\nbasename \"$1\" > \"$1\"\n").unwrap();
    std::fs::set_permissions(&fake, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let run = || {
        let out = Command::new(GATE).args(["run", "s", "-j", "1", "--bin"]).arg(&fake).current_dir(&root).env("REGATE_QUEUE_USER", "t").output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        assert!(out.status.success(), "{text}");
        let md5 = |name: &str| text.lines().find(|l| l.starts_with(name)).and_then(|l| l.split_whitespace().nth(9)).unwrap_or_default().to_string();
        (md5("r.xlsx"), md5("r.pdf"), text.matches("(cached)").count())
    };
    let (xlsx, pdf, _) = run();
    assert_ne!(xlsx, pdf);
    assert_eq!(run(), (xlsx, pdf, 2), "each case is taken from its own record");
    std::fs::remove_dir_all(d).unwrap();
}

/// A job started by a relative path (`../build.sh` from a worktree) must stay in the queue
/// for clients in any other directory: its lock path was stored as given, they could not
/// open it, took it for dead and gave its place away.
#[test]
fn a_job_started_by_a_relative_config_is_seen_from_anywhere() {
    let d = dir("relative", "[total]\njobs = 1\nmin_available = 0\n[heavy]\njob_memory = 1G\n");
    let mut job = Command::new(GATE)
        .args(["queue", "--no-scope", "--config", "limits.conf", "--", "sleep", "60"])
        .current_dir(&d)
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let status = || {
        let out = Command::new(GATE).args(["queue", "status", "--config"]).arg(d.join("limits.conf")).current_dir("/").output().unwrap();
        String::from_utf8_lossy(&out.stdout).matches("running").count()
    };
    // Stays seen, not seen once: the first look from elsewhere is what dropped it.
    let t = Instant::now();
    while status() == 0 {
        assert!(t.elapsed() < Duration::from_secs(5), "the job fell out of the queue");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(status(), 1, "the job fell out of the queue");
    unsafe { libc::kill(-(job.id() as i32), libc::SIGKILL) };
    job.wait().unwrap();
    std::fs::remove_dir_all(d).unwrap();
}

/// An id is never given twice, even once the queue is empty again: two jobs under one id
/// started each other's entry, and the one left behind held a place for good.
#[test]
fn a_client_marked_started_but_still_waiting_gets_its_turn_back() {
    let d = dir("stuck", "[total]\njobs = 1\nmin_available = 0\n[heavy]\njob_memory = 1G\n");
    let mut holder = queue(&d, "heavy", "a", "1G", false, "sleep 60");
    // The holder in first: two clients at once race for the queue's lock, and under load the waiter wins.
    let t = Instant::now();
    loop {
        let text = std::fs::read_to_string(d.join("queue.json")).unwrap_or_default();
        let now: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
        if now.iter().any(|e| e["running"] == true) {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "the holder never started: {text}");
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut waiter = queue(&d, "heavy", "b", "1G", false, "echo ran");
    std::thread::sleep(Duration::from_millis(600));
    // The waiter, not the job: its entry is marked started as the old clients did to each other.
    let state = d.join("queue.json");
    let mut entries: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(&state).unwrap()).unwrap();
    assert_eq!(entries.len(), 2, "{entries:?}");
    for e in &mut entries {
        if e["pid"].as_i64() == Some(waiter.id() as i64) {
            e["running"] = true.into();
            e["since"] = 1.into();
        }
    }
    std::fs::write(&state, serde_json::to_string(&entries).unwrap()).unwrap();
    // The holder ends: with the mark left, the waiter would wait for good.
    unsafe { libc::kill(-(holder.id() as i32), libc::SIGKILL) };
    holder.wait().unwrap();
    let t = Instant::now();
    while waiter.try_wait().unwrap().is_none() {
        assert!(t.elapsed() < Duration::from_secs(10), "the waiter never got its turn back");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn a_waiting_job_is_cancelled_by_its_owner_or_by_force_and_nothing_else() {
    let d = dir("cancel", "[total]\njobs = 1\nmin_available = 0\n[heavy]\njob_memory = 1G\n");
    let conf = d.join("limits.conf");
    let cancel = |args: &[&str]| Command::new(GATE).args(["queue", "cancel"]).args(args).arg("--config").arg(&conf).env_remove("REGATE_QUEUE_USER").output().unwrap();
    let id_of = |pid: u32| -> (String, bool) {
        let t = Instant::now();
        loop {
            let text = std::fs::read_to_string(d.join("queue.json")).unwrap_or_default();
            let now: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
            if let Some(e) = now.iter().find(|e| e["pid"].as_i64() == Some(pid as i64)) {
                return (e["id"].to_string(), e["running"] == true);
            }
            assert!(t.elapsed() < Duration::from_secs(10), "{pid} never queued: {text}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    let mut holder = queue(&d, "heavy", "a", "1G", false, "sleep 60");
    while !id_of(holder.id()).1 {
        std::thread::sleep(Duration::from_millis(10));
    }
    let ran = d.join("ran");
    let script = format!("touch {}", ran.display());
    let (mut mine, mut theirs) = (queue(&d, "heavy", "b", "1G", false, &script), queue(&d, "heavy", "c", "1G", false, &script));
    let (mine_id, theirs_id, holder_id) = (id_of(mine.id()).0, id_of(theirs.id()).0, id_of(holder.id()).0);
    let said = |o: &std::process::Output| String::from_utf8_lossy(&o.stderr).to_string();

    let o = cancel(&[&holder_id, "--user", "a"]);
    assert!(!o.status.success() && said(&o).contains("is running"), "{}", said(&o));
    let o = cancel(&["99999", "--user", "b"]);
    assert!(!o.status.success() && said(&o).contains("no job"), "{}", said(&o));
    let o = cancel(&[&theirs_id, "--user", "b"]);
    assert!(!o.status.success() && said(&o).contains("--force"), "{}", said(&o));
    assert!(theirs.try_wait().unwrap().is_none(), "another's job untouched");

    assert!(cancel(&[&mine_id, "--user", "b"]).status.success());
    assert!(cancel(&[&theirs_id, "--user", "b", "--force"]).status.success());
    assert!(!mine.wait().unwrap().success());
    assert!(!theirs.wait().unwrap().success());
    unsafe { libc::kill(-(holder.id() as i32), libc::SIGKILL) };
    holder.wait().unwrap();

    // The place is free and neither ran: the next job goes straight in, and `cancel` queued nothing of its own.
    let mut next = queue(&d, "heavy", "z", "1G", false, "true");
    let t = Instant::now();
    while next.try_wait().unwrap().is_none() {
        assert!(t.elapsed() < Duration::from_secs(10), "cancelled jobs still hold the queue");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!ran.exists(), "a cancelled job ran");
    assert_eq!(std::fs::read_to_string(d.join("queue.next")).unwrap().trim().parse::<u64>().unwrap(), 4, "ids: holder, two waiters, next");
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn job_ids_are_never_reused() {
    let d = dir("ids", "[total]\nmin_available = 0\n[heavy]\njob_memory = 1G\n");
    let id = || {
        let out = queue(&d, "heavy", "a", "1G", false, "echo $REGATE_QUEUE_JOB").wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().unwrap()
    };
    let (first, second, third) = (id(), id(), id());
    assert!(first < second && second < third, "{first} {second} {third}");
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn a_scope_gets_the_memory_and_cpu_caps() {
    let d = dir("scope", "[total]\nmin_available = 0\n[heavy]\njob_memory = 1G\njob_threads = 3\n");
    let script = "g=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup); cat $g/memory.max $g/memory.swap.max $g/cpu.max";
    let out = queue(&d, "heavy", "a", "1G", true, script).wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, ["1073741824", "0", "300000 100000"], "{text}");
    // A job no limit can hold is refused at once, not queued forever.
    let out = Command::new(GATE)
        .args(["queue", "--config"])
        .arg(d.join("limits.conf"))
        .args(["--kind", "nope", "--", "true"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    std::fs::write(d.join("limits.conf"), "[total]\nmemory = 1G\n[heavy]\njob_memory = 2G\n").unwrap();
    let out = Command::new(GATE).args(["queue", "--config"]).arg(d.join("limits.conf")).args(["--", "true"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("exceeds the machine total"));
    // max_runtime: systemd ends the scope; the job fails instead of running on.
    std::fs::write(d.join("limits.conf"), "[total]\nmin_available = 0\n[heavy]\njob_memory = 1G\nmax_runtime = 1\n").unwrap();
    let t = Instant::now();
    assert!(!queue(&d, "heavy", "a", "1G", true, "sleep 20").wait().unwrap().success());
    assert!(t.elapsed() < Duration::from_secs(8), "ran {:?} under max_runtime = 1", t.elapsed());
    // min_disk: no room on disk, the job waits and says why.
    std::fs::write(d.join("limits.conf"), "[total]\nmin_disk = 100000000G\n[heavy]\njob_memory = 1G\n").unwrap();
    let mut c = Command::new(GATE)
        .args(["queue", "--no-scope", "--config"])
        .arg(d.join("limits.conf"))
        .args(["--", "true"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(c.stderr.take().unwrap()), &mut first).unwrap();
    // status names the same wait: it used to ask only the memory and say "N MB free".
    let status = Command::new(GATE).args(["queue", "status", "--config"]).arg(d.join("limits.conf")).output().unwrap();
    c.kill().unwrap();
    c.wait().unwrap();
    assert!(first.contains("below min_disk"), "{first}");
    let status = String::from_utf8_lossy(&status.stdout);
    assert!(status.contains("waits for: ") && status.contains("below min_disk"), "{status}");
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn queue_help_prints_the_usage_and_an_unknown_option_is_refused() {
    let out = Command::new(GATE).args(["queue", "--help"]).output().unwrap();
    assert!(out.status.success() && String::from_utf8_lossy(&out.stdout).contains("regate queue status"));
    let out = Command::new(GATE).args(["queue", "--nope", "true"]).output().unwrap();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("unknown option --nope"));
}

/// A reference held worse fails both `run` and `diff`, even when the output bytes agree; held better passes.
/// Both used to exit 0: `run` printed LESS ACCURATE as a word, `diff` compared only the bytes.
#[test]
fn accuracy_lost_fails_run_and_diff() {
    let d = dir("accuracy", "[total]\nmin_available = 0\n[heavy]\njob_memory = 100M\n[user]\njobs = 9\n");
    let root = d.join("root");
    std::fs::create_dir_all(root.join(".regate")).unwrap();
    std::fs::create_dir_all(root.join("tests")).unwrap();
    std::fs::rename(d.join("limits.conf"), root.join(".regate/limits.conf")).unwrap();
    std::fs::write(root.join("tests/gate.cases"), "[s] mem=100M\nc.csv $out\n").unwrap();
    assert!(Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap().success());
    let fake = |name: &str, error: f64| {
        let p = d.join(name);
        let script = format!("#!/bin/sh\necho same > \"$1\"\necho '{{\"name\": \"ref q 1\", \"value\": {error}}}' > \"$REGATE_MEASURES\"\n");
        std::fs::write(&p, script).unwrap();
        std::fs::set_permissions(&p, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        p
    };
    let (good, bad) = (fake("good", 1.0), fake("bad", 2.0));
    let gate = |args: &[&std::ffi::OsStr]| Command::new(GATE).args(args).current_dir(&root).env("REGATE_QUEUE_USER", "t").output().unwrap();
    let diff = gate(&["diff".as_ref(), good.as_os_str(), bad.as_os_str(), "s".as_ref(), "-j".as_ref(), "1".as_ref()]);
    let text = String::from_utf8_lossy(&diff.stdout);
    assert!(!diff.status.success() && text.contains("LESS ACCURATE ref q 1 1.00 -> 2.00 %") && text.contains("hold a reference worse"), "{text}");
    let back = gate(&["diff".as_ref(), bad.as_os_str(), good.as_os_str(), "s".as_ref(), "-j".as_ref(), "1".as_ref()]);
    let text = String::from_utf8_lossy(&back.stdout);
    assert!(back.status.success() && text.contains("more accurate"), "{text}");
    // `run` judges against the binaries before it in history: good and bad both ran above, bad is the worse.
    let run = gate(&["run".as_ref(), "s".as_ref(), "--bin".as_ref(), bad.as_os_str()]);
    let text = String::from_utf8_lossy(&run.stdout);
    assert!(!run.status.success() && text.contains("LESS ACCURATE"), "{text}");
    std::fs::remove_dir_all(d).unwrap();
}

#[test]
fn init_writes_what_is_missing_and_keeps_the_rest() {
    let d = dir("init", "");
    let root = d.join("root");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"app\"\n").unwrap();
    assert!(Command::new("git").args(["init", "-q"]).current_dir(&root).status().unwrap().success());
    let init = || String::from_utf8(Command::new(GATE).arg("init").current_dir(&root).output().unwrap().stdout).unwrap();
    assert_eq!(init().matches("wrote").count(), 3);
    assert!(std::fs::read_to_string(root.join("gate.conf")).unwrap().contains("bin   = ../target/release/app"));
    assert_eq!(std::fs::read_to_string(root.join(".regate/.gitignore")).unwrap(), "*\n");
    assert_eq!(init().matches("kept").count(), 3, "a second init changes nothing");
    let list = Command::new(GATE).arg("list").current_dir(&root).output().unwrap();
    assert!(list.status.success(), "{}", String::from_utf8_lossy(&list.stderr));
    std::fs::remove_dir_all(d).unwrap();
}
