//! One child process, measured: wall time and CPU from the moment the queue handed over to the
//! measured program, peak RSS from wait4, and how it ended.

use std::io;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub struct Outcome {
    /// "ok", "exit N", "signal N (NAME)" or "timeout".
    pub status: String,
    pub wall_s: f64,
    pub cpu_s: f64,
    pub rss_mb: f64,
    /// From the spawn to the queue's mark: what the job stood in line, 0 without a marker.
    pub wait_s: f64,
}

/// Where the queue says it handed over: "<unix ns> <cpu s so far>", read by the run that set it.
pub const STARTED_ENV: &str = "REGATE_QUEUE_STARTED";

fn unix_ns() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

fn secs(t: libc::timeval) -> f64 {
    t.tv_sec as f64 + t.tv_usec as f64 / 1e6
}

/// The queue calls this just before exec. The same PID carries on, so wait4 will count its
/// waiting too; written by rename, so a reader sees all of it or nothing.
pub fn mark_started() {
    let Some(path) = crate::env("QUEUE_STARTED") else { return };
    let mut cpu = 0.0;
    for who in [libc::RUSAGE_SELF, libc::RUSAGE_CHILDREN] {
        let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
        unsafe { libc::getrusage(who, &mut ru) };
        cpu += secs(ru.ru_utime) + secs(ru.ru_stime);
    }
    let mut tmp = path.clone();
    tmp.push(".tmp");
    if let Err(e) = std::fs::write(&tmp, format!("{} {cpu}", unix_ns())).and_then(|_| std::fs::rename(&tmp, &path)) {
        eprintln!("queue: {}: {e}; the wait will count as the job's time", tmp.display());
    }
}

fn read_mark(path: &Path) -> Option<(u128, f64)> {
    let text = std::fs::read_to_string(path).ok()?;
    let (ns, cpu) = text.split_once(' ')?;
    Some((ns.parse().ok()?, cpu.parse().ok()?))
}

/// Runs `cmd` to its end. With `marker`, the clocks start when the queue marks that file: a job
/// waits for a free slot and then execs in the same PID, and the queue is not the program's
/// time, however short the program. The timeout counts from the same moment.
pub fn run(cmd: &mut Command, marker: Option<&Path>, timeout: Duration) -> io::Result<Outcome> {
    cmd.process_group(0);
    if let Some(m) = marker {
        let _ = std::fs::remove_file(m);
        cmd.env(STARTED_ENV, m);
    }
    let spawned = Instant::now();
    let spawned_ns = unix_ns();
    let child = cmd.spawn()?;
    let pid = child.id() as libc::pid_t;
    let mut started = if marker.is_none() { Some(spawned) } else { None };
    let mut timed_out = false;
    let mut status: libc::c_int = 0;
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        let r = unsafe { libc::wait4(pid, &mut status, libc::WNOHANG, &mut ru) };
        if r == pid {
            break;
        }
        if r < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if started.is_none() && marker.is_some_and(|m| read_mark(m).is_some()) {
            started = Some(Instant::now());
        }
        if let Some(s) = started
            && !timed_out
            && s.elapsed() > timeout
        {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            timed_out = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // Reaped by wait4 above; dropping the handle does not wait again.
    drop(child);
    let mark = marker.and_then(read_mark);
    if let Some(m) = marker {
        let _ = std::fs::remove_file(m);
    }
    let status = if timed_out {
        "timeout".to_string()
    } else if libc::WIFEXITED(status) {
        match libc::WEXITSTATUS(status) {
            0 => "ok".into(),
            n => format!("exit {n}"),
        }
    } else if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        let name = match sig {
            libc::SIGABRT => "SIGABRT",
            libc::SIGSEGV => "SIGSEGV",
            libc::SIGBUS => "SIGBUS",
            libc::SIGILL => "SIGILL",
            libc::SIGKILL => "SIGKILL: memory cap or OOM",
            _ => "",
        };
        format!("signal {sig} ({name})")
    } else {
        format!("wait status {status}")
    };
    Ok(Outcome {
        status,
        wall_s: mark.map_or(spawned.elapsed().as_secs_f64(), |(ns, _)| unix_ns().saturating_sub(ns) as f64 / 1e9),
        cpu_s: (secs(ru.ru_utime) + secs(ru.ru_stime) - mark.map_or(0.0, |(_, cpu)| cpu)).max(0.0),
        rss_mb: ru.ru_maxrss as f64 / 1024.0,
        wait_s: mark.map_or(0.0, |(ns, _)| ns.saturating_sub(spawned_ns) as f64 / 1e9),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_signal_timeout_and_memory() {
        let sh = |s: &str| {
            let mut c = Command::new("sh");
            c.args(["-c", s]);
            c
        };
        let long = Duration::from_secs(30);
        assert_eq!(run(&mut sh("exit 0"), None, long).unwrap().status, "ok");
        assert_eq!(run(&mut sh("exit 3"), None, long).unwrap().status, "exit 3");
        assert!(run(&mut sh("kill -SEGV $$"), None, long).unwrap().status.starts_with("signal 11"));
        let t = run(&mut sh("sleep 10"), None, Duration::from_millis(200)).unwrap();
        assert_eq!(t.status, "timeout");
        assert!(t.wall_s < 5.0);
        // dd fills a 200 MB buffer in a child the shell waits for: wait4 reports the largest descendant.
        let m = run(&mut sh("dd if=/dev/zero of=/dev/null bs=200M count=1 2>/dev/null"), None, long).unwrap();
        assert!(m.rss_mb > 150.0, "rss {}", m.rss_mb);
    }

    #[test]
    fn clocks_start_at_the_mark_even_for_a_program_gone_in_an_instant() {
        let dir = std::env::temp_dir().join(format!("gate-exec-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("started");
        // Half a second of queue with half a second of CPU in it, then a program that exits at once: no poll sees it run.
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 0.5; printf \"$(date +%s%N) 0.5\" > \"$REGATE_QUEUE_STARTED\"; exit 3"]);
        let o = run(&mut c, Some(&marker), Duration::from_secs(30)).unwrap();
        assert_eq!(o.status, "exit 3");
        assert!(o.wall_s < 0.3, "wall {}", o.wall_s);
        assert!(o.cpu_s < 0.1, "cpu {}", o.cpu_s);
        assert!(o.wait_s >= 0.4, "wait {}", o.wait_s);
        assert!(!marker.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
