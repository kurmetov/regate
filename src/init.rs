//! `regate init`: a gate.conf, a case file and a limits.conf sized to this machine, each only where none is.

use std::path::Path;

/// The template's limits are for this machine; init scales its sizes to the one it runs on.
const TEMPLATE: &str = include_str!("../limits.conf");
const TEMPLATE_MB: f64 = 59.0 * 1024.0;
const TEMPLATE_THREADS: f64 = 32.0;

const CASES: &str = "\
# regate cases. `let` names an input (a `*` makes one case per match); `[set]` starts a set; then one case a line:
# <output name>  <arguments to the binary>, with $out the output file and $name a `let` input. Attributes after
# the set name or the case: mem=4G timeout=10m repeat=3 slower=25% grow=15%.
#
# let small = data/small.csv
# let every = data/inputs/*.csv
[quick] mem=2G
# small.json   convert $small -o $out
# every.json   convert $every -o $out
";

pub fn main(root: &Path, data: &Path) -> Result<i32, String> {
    let conf = root.join(crate::project::FILE);
    write_new(&conf, &gate_conf(root))?;
    let project = crate::project::Project::load(root)?;
    write_new(&root.join(&project.cases), CASES)?;
    let (mb, threads) = machine();
    write_new(&data.join("limits.conf"), &limits(mb, threads))?;
    println!(
        "next: name the cases in {}, then `regate run`. The queue is one per limits.conf: on a machine several projects \
         share, keep one and give it to all of them (`regate queue --config FILE`). Guard rules for Claude Code: see the README.",
        project.cases.display()
    );
    Ok(0)
}

fn write_new(path: &Path, text: &str) -> Result<(), String> {
    if path.exists() {
        println!("kept    {} (already there)", path.display());
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    println!("wrote   {}", path.display());
    Ok(())
}

/// A Cargo package builds as is; anything else gets the keys to fill in.
fn gate_conf(root: &Path) -> String {
    let head = "# regate: how to build this project (every key is in the README).\n";
    let package = std::fs::read_to_string(root.join("Cargo.toml")).ok().and_then(|t| package_name(&t));
    match package {
        Some(name) => format!(
            "{head}build = cargo build --release\nbin   = ../target/release/{name}   # relative to the build worktree; ../target is the gate's own\n"
        ),
        None => format!("{head}# build = make           # run in the build worktree <data>/wt, split on spaces\n# bin   = out/app        # what the build made, relative to that worktree\n"),
    }
}

/// `name` of `[package]`; a workspace root without one has no single binary to guess.
fn package_name(toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in toml.lines().map(str::trim) {
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if let Some(v) = line.strip_prefix("name").map(str::trim_start).and_then(|v| v.strip_prefix('=')).filter(|_| in_package) {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

/// MemTotal in MB and the threads this process may use.
fn machine() -> (u64, u64) {
    let mb = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| m.lines().find_map(|l| l.strip_prefix("MemTotal:")?.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()))
        .map_or(TEMPLATE_MB as u64, |kb| kb / 1024);
    (mb, std::thread::available_parallelism().map_or(TEMPLATE_THREADS as u64, |n| n.get() as u64))
}

/// The template with its memory and thread sizes scaled from its machine to one of `mb` and `threads`.
fn limits(mb: u64, threads: u64) -> String {
    let (m, t) = (mb as f64 / TEMPLATE_MB, threads as f64 / TEMPLATE_THREADS);
    let mut out = String::new();
    for line in TEMPLATE.lines() {
        let line = if line.starts_with("# regate queue limits") {
            "# regate queue limits, written by regate init for this machine: tune them.".to_string()
        } else {
            line.replace("(here 59 GB, 32 threads)", &format!("(here {} GB, {threads} threads)", mb / 1024))
        };
        let (body, comment) = line.split_once('#').map_or((line.as_str(), None), |(b, c)| (b, Some(c)));
        // The new value where the old one was, padded to its width so the comments stay in line.
        let scaled = body.split_once('=').and_then(|(k, v)| {
            let v = v.trim();
            let n = match k.trim() {
                "memory" | "min_available" | "max_caps" | "job_memory" => format!("{}G", ((v.strip_suffix('G')?.parse::<f64>().ok()? * m).round() as u64).max(1)),
                "threads" | "job_threads" => ((v.parse::<f64>().ok()? * t).round() as u64).max(1).to_string(),
                _ => return None,
            };
            let at = body.find('=')? + 1;
            Some(format!("{}{}", &body[..at], body[at..].replacen(v, &format!("{n:<w$}", w = v.len()), 1)))
        });
        out += &match (scaled, comment) {
            (Some(b), Some(c)) => format!("{b}#{c}\n"),
            (Some(b), None) => format!("{}\n", b.trim_end()),
            (None, _) => format!("{line}\n"),
        };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_scale_to_the_machine() {
        assert_eq!(limits(TEMPLATE_MB as u64, 32).lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>(), TEMPLATE.lines().filter(|l| !l.starts_with('#')).collect::<Vec<_>>(), "the template's own machine changes no value");
        let small = crate::queue::Limits::parse(&limits(16 * 1024, 8)).unwrap();
        assert_eq!((small.total.mem_mb, small.total.threads), (Some(9 * 1024), Some(12)), "34G and 48 threads at 16 GB, 8 threads");
        assert_eq!(small.kinds["light"].job_mem_mb, Some(1024), "never below 1G");
        assert!(limits(16 * 1024, 8).contains("(here 16 GB, 8 threads)"));
    }

    #[test]
    fn a_cargo_package_builds_as_is_and_the_cases_parse() {
        assert_eq!(package_name("[workspace]\nmembers = [\"a\"]\n"), None);
        assert_eq!(package_name("[package]\nname = \"app\"\n[dependencies]\nname = \"x\"\n").as_deref(), Some("app"));
        let sets = crate::cases::parse(CASES, Path::new("/r"), &Default::default()).unwrap();
        assert_eq!(sets.get(None).unwrap().0, "quick");
    }
}
