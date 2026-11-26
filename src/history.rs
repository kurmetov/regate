//! `<data>/history.jsonl`: one line per build and per case actually run, never rewritten.
//! It is the result cache too: a case whose key matches an earlier ok line is not rerun.

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Rec {
    /// Unix seconds when it started.
    pub ts: u64,
    /// "case" or "build".
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub set: String,
    /// md5 of the project's binary (for a build, of the one it produced).
    pub bin: String,
    /// What was built: a git tree id, or the path given with --bin.
    pub rev: String,
    #[serde(default)]
    pub key: String,
    pub status: String,
    pub wall_s: f64,
    pub cpu_s: f64,
    pub rss_mb: f64,
    /// md5 over (file, size, md5) of every output file; empty unless ok.
    #[serde(default)]
    pub out: String,
    #[serde(default)]
    pub files: Vec<(String, u64, String)>,
    /// 1-minute load average when it started: a slow run on a busy box reads as such.
    #[serde(default)]
    pub load: f64,
    #[serde(default)]
    pub log: String,
    /// CPU threads the job was allowed: changes both its CPU time and, where the program lets it, its bytes.
    #[serde(default)]
    pub threads: u64,
    /// Runs the numbers are the least of.
    #[serde(default)]
    pub repeat: u32,
    /// CPU seconds of each run: their spread is how noisy the box was just now.
    #[serde(default)]
    pub cpu_runs: Vec<f64>,
    /// Seconds in the queue, over all repeats. Not in wall_s or cpu_s.
    #[serde(default)]
    pub wait_s: f64,
    /// Seconds from `ts` to the end, queue and repeats included.
    #[serde(default)]
    pub took_s: f64,
    /// One launch of a set. One binary can be launched hours apart, so the binary does not tell a run's length.
    #[serde(default)]
    pub run: String,
    /// The machine it ran on (remote.conf's destination); empty for this one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub host: String,
    /// What the run measured against a reference the program did not make, as it wrote them to
    /// REGATE_MEASURES: each a share off that reference in %, lower is better.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub measures: std::collections::BTreeMap<String, f64>,
}

pub fn load(path: &Path) -> Vec<Rec> {
    let Ok(text) = std::fs::read_to_string(path) else { return vec![] };
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

pub fn append(path: &Path, rec: &Rec) -> std::io::Result<()> {
    let mut line = serde_json::to_string(rec).expect("plain struct serializes");
    line.push('\n');
    // One write of one line with O_APPEND: parallel runs do not interleave inside a line.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

/// What a run is judged against: the median over the last five other binaries' latest ok
/// run of the same case with the same threads and repeats (others are not comparable), and
/// the output of the most recent of them.
pub struct Baseline {
    pub cpu_s: f64,
    pub rss_mb: f64,
    pub out: String,
    pub measures: std::collections::BTreeMap<String, f64>,
}

/// `<tree>.<profile>`: a binary built otherwise says nothing about this one's CPU.
fn profile(rev: &str) -> &str {
    rev.rsplit_once('.').map_or("", |r| r.1)
}

pub fn baseline(hist: &[Rec], rec: &Rec) -> Option<Baseline> {
    let (kind, name, bin) = (rec.kind.as_str(), rec.name.as_str(), rec.bin.as_str());
    let mut seen: Vec<&str> = vec![];
    let mut picked: Vec<&Rec> = vec![];
    for r in hist.iter().rev() {
        if r.kind != kind || r.name != name || r.status != "ok" || r.bin == bin || r.threads != rec.threads || r.repeat != rec.repeat || profile(&r.rev) != profile(&rec.rev) || r.host != rec.host {
            continue;
        }
        if seen.contains(&r.bin.as_str()) {
            continue;
        }
        seen.push(&r.bin);
        picked.push(r);
        if picked.len() == 5 {
            break;
        }
    }
    let first = picked.first()?;
    Some(Baseline {
        cpu_s: median(picked.iter().map(|r| r.cpu_s).collect()),
        rss_mb: median(picked.iter().map(|r| r.rss_mb).collect()),
        out: first.out.clone(),
        measures: first.measures.clone(),
    })
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    let n = v.len();
    if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 }
}

/// One line per launch, the latest `last`: when, set, cases, minutes start to end, minutes of the longest
/// queue wait and its case. Older lines have no `run` and are left out: nothing says where they belong.
pub fn runs(hist: &[Rec], last: usize) -> Vec<(u64, String, usize, f64, f64, String)> {
    let mut ids: Vec<&str> = vec![];
    for r in hist.iter().filter(|r| !r.run.is_empty()) {
        if !ids.contains(&r.run.as_str()) {
            ids.push(&r.run);
        }
    }
    ids[ids.len().saturating_sub(last)..]
        .iter()
        .map(|id| {
            let rows: Vec<&Rec> = hist.iter().filter(|r| r.run == *id).collect();
            let start = rows.iter().map(|r| r.ts).min().unwrap_or(0);
            let end = rows.iter().map(|r| r.ts as f64 + r.took_s).fold(0.0, f64::max);
            let worst = rows.iter().max_by(|a, b| a.wait_s.total_cmp(&b.wait_s)).expect("an id comes from a row");
            (start, rows[0].set.clone(), rows.len(), (end - start as f64) / 60.0, worst.wait_s / 60.0, worst.name.clone())
        })
        .collect()
}

/// A run that says nothing about the binary: an input missing on this machine. A stale case (kept of another
/// input) is no such run: it checked nothing, and two of them read as "same" hid a blind set for hours.
pub fn unjudged(status: &str) -> bool {
    status.starts_with("skipped")
}

/// The code failed: not a run that judged nothing, and not the gate itself failing to run it.
pub fn failed(status: &str) -> bool {
    status != "ok" && !unjudged(status) && !status.starts_with("gate error")
}

/// Words for how `rec` sits against `base`. CPU, not wall time: the box is shared and wall
/// time moves with whoever else is running. Small absolute moves are noise, not findings.
/// `th` is (slower, grow): the percent more CPU / peak memory that counts.
pub fn verdict(rec: &Rec, base: &Baseline, th: (f64, f64)) -> Vec<String> {
    let mut v = vec![];
    let pct = |a: f64, b: f64| (a / b - 1.0) * 100.0;
    // Runs that differ this much among themselves cannot show a smaller move.
    let (least, most) = rec.cpu_runs.iter().fold((f64::MAX, 0f64), |(l, m), &c| (l.min(c), m.max(c)));
    let spread = if least > 0.0 && least < f64::MAX { pct(most, least) } else { 0.0 };
    let (cpu, mem) = (1.0 + th.0.max(spread) / 100.0, 1.0 + th.1 / 100.0);
    if rec.cpu_s > base.cpu_s * cpu && rec.cpu_s - base.cpu_s > 1.0 {
        v.push(format!("SLOWER +{:.0}% cpu", pct(rec.cpu_s, base.cpu_s)));
    } else if rec.cpu_s * cpu < base.cpu_s && base.cpu_s - rec.cpu_s > 1.0 {
        v.push(format!("faster {:.0}% cpu", pct(rec.cpu_s, base.cpu_s)));
    }
    if rec.rss_mb > base.rss_mb * mem && rec.rss_mb - base.rss_mb > 100.0 {
        v.push(format!("MORE MEMORY +{:.0}%", pct(rec.rss_mb, base.rss_mb)));
    } else if rec.rss_mb * mem < base.rss_mb && base.rss_mb - rec.rss_mb > 100.0 {
        v.push(format!("less memory {:.0}%", pct(rec.rss_mb, base.rss_mb)));
    }
    if rec.kind == "case" && !rec.out.is_empty() && rec.out != base.out {
        v.push("output changed".into());
    }
    v.extend(accuracy(&rec.measures, &base.measures));
    v
}

/// How each error against a reference moved from `was` to `now`. Unlike time, these do not move with the box.
pub fn accuracy(now: &std::collections::BTreeMap<String, f64>, was: &std::collections::BTreeMap<String, f64>) -> Vec<String> {
    let mut v = vec![];
    // Moves under 0.05 points are the printing's rounding, not the program.
    for (name, &n) in now {
        match was.get(name) {
            Some(&w) if n > w + 0.05 => v.push(format!("LESS ACCURATE {name} {w:.2} -> {n:.2} %")),
            Some(&w) if n < w - 0.05 => v.push(format!("more accurate {name} {w:.2} -> {n:.2} %")),
            None if !was.is_empty() => v.push(format!("new measure {name} {n:.2} %")),
            _ => {}
        }
    }
    for name in was.keys().filter(|n| !now.contains_key(*n)) {
        v.push(format!("MEASURE GONE {name}"));
    }
    v
}

/// Words of `verdict`/`accuracy` that fail a run: a reference held worse or no longer held. Time and memory
/// only warn, a shared box moves them by itself.
pub fn worse(words: &[String]) -> bool {
    words.iter().any(|w| w.starts_with("LESS ACCURATE") || w.starts_with("MEASURE GONE"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(bin: &str, cpu: f64, rss: f64, out: &str) -> Rec {
        Rec {
            kind: "case".into(),
            name: "small.json".into(),
            bin: bin.into(),
            status: "ok".into(),
            cpu_s: cpu,
            rss_mb: rss,
            out: out.into(),
            ..Default::default()
        }
    }

    #[test]
    fn baseline_takes_latest_per_other_binary() {
        let hist = vec![
            rec("a", 10.0, 1000.0, "x"),
            rec("b", 12.0, 1000.0, "x"),
            rec("b", 11.0, 1000.0, "y"), // later run of b wins
            Rec { status: "signal 11 (SIGSEGV)".into(), ..rec("c", 1.0, 1.0, "") },
            rec("new", 99.0, 9999.0, "z"), // the binary under test is left out
        ];
        let b = baseline(&hist, &rec("new", 0.0, 0.0, "")).unwrap();
        assert_eq!((b.cpu_s, b.out.as_str()), (10.5, "y"));
        let v = verdict(&rec("new", 14.0, 1300.0, "z"), &b, (15.0, 10.0));
        assert_eq!(v, ["SLOWER +33% cpu", "MORE MEMORY +30%", "output changed"]);
        assert!(verdict(&rec("new", 10.6, 1050.0, "y"), &b, (15.0, 10.0)).is_empty());
        // A case allowed 50% / 40% does not flag the same run.
        assert_eq!(verdict(&rec("new", 14.0, 1300.0, "z"), &b, (50.0, 40.0)), ["output changed"]);
        // Repeats 40% apart hide a +33% move: that is noise, not a finding.
        let noisy = Rec { cpu_runs: vec![14.0, 19.6], ..rec("new", 14.0, 1000.0, "y") };
        assert!(verdict(&noisy, &b, (15.0, 10.0)).is_empty());
        // Other threads or repeats are not a baseline.
        assert!(baseline(&hist, &Rec { threads: 8, ..rec("new", 0.0, 0.0, "") }).is_none());
    }

    #[test]
    fn accuracy_moves_are_named() {
        let m = |pairs: &[(&str, f64)]| pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect();
        let base = Rec { measures: m(&[("ref moves 1", 1.2), ("ref turns 1", 3.0), ("ref q 1", 40.0)]), ..rec("a", 10.0, 1000.0, "x") };
        let b = baseline(&[base], &rec("new", 0.0, 0.0, "")).unwrap();
        let now = Rec { measures: m(&[("ref moves 1", 1.5), ("ref turns 1", 2.0), ("ref new 1", 5.0)]), ..rec("new", 10.0, 1000.0, "y") };
        assert_eq!(
            verdict(&now, &b, (15.0, 10.0)),
            ["output changed", "LESS ACCURATE ref moves 1 1.20 -> 1.50 %", "new measure ref new 1 5.00 %", "more accurate ref turns 1 3.00 -> 2.00 %", "MEASURE GONE ref q 1"]
        );
        // Rounding of the print is not a move.
        let same = Rec { measures: m(&[("ref moves 1", 1.23), ("ref turns 1", 3.0), ("ref q 1", 40.0)]), ..rec("new", 10.0, 1000.0, "x") };
        assert!(verdict(&same, &b, (15.0, 10.0)).is_empty());
        // Accuracy lost or a measure gone fails; slower, more memory or more accurate do not.
        assert!(worse(&verdict(&now, &b, (15.0, 10.0))));
        assert!(worse(&accuracy(&m(&[("ref moves 1", 1.2), ("ref turns 1", 3.0)]), &b.measures)));
        assert!(!worse(&["SLOWER +33% cpu".into(), "MORE MEMORY +30%".into(), "more accurate x 3.00 -> 2.00 %".into(), "output changed".into()]));
    }

    #[test]
    fn a_launch_is_timed_on_its_own_however_old_its_binary() {
        let row = |run: &str, name: &str, ts, took_s, wait_s| Rec { run: run.into(), name: name.into(), set: "s".into(), ts, took_s, wait_s, ..rec("a", 1.0, 1.0, "x") };
        let hist = vec![
            row("", "old", 0, 1.0, 0.0),
            row("r1", "a", 100, 60.0, 0.0),
            row("r1", "b", 110, 170.0, 120.0), // last to start, last to end: its own time counts
            row("r2", "a", 7300, 60.0, 0.0),
        ];
        let got = runs(&hist, 10);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], (100, "s".to_string(), 2, 3.0, 2.0, "b".to_string()));
        assert_eq!(got[1].3, 1.0);
        assert_eq!(runs(&hist, 1).len(), 1);
    }

    #[test]
    fn history_round_trip() {
        let dir = std::env::temp_dir().join(format!("gate-hist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("h.jsonl");
        append(&path, &rec("a", 1.0, 2.0, "x")).unwrap();
        append(&path, &rec("b", 3.0, 4.0, "y")).unwrap();
        std::fs::write(dir.join("junk"), "").unwrap();
        let back = load(&path);
        assert_eq!(back.len(), 2);
        assert_eq!(back[1].bin, "b");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
