//! The case file (`tests/gate.cases` unless gate.conf names another):
//!
//! ```text
//! # comment
//! let small = data/small.csv                 # a path relative to the project root
//! let big = "data/with spaces/big.csv"       # quoted when it holds spaces
//!
//! [quick]                                    # a set; the first one is the default
//! small.json  convert --to json $small -o $out
//!
//! [full] timeout=3600                       # attributes on a set line: its cases' defaults
//! include quick                              # every case of an earlier set
//! big.db  mem=16G repeat=3 slower=25%  import $big -o $out
//! ```
//!
//! Attributes: `mem=12G` (memory cap), `timeout=1800` (s), `repeat=1` (runs; the least CPU
//! and peak memory are kept, outputs must agree), `slower=15%` / `grow=10%` (how much more
//! CPU / peak memory than the baseline counts as a regression).
//!
//! `$name` is replaced only as a whole argument. `$out` is the case's output, named after
//! the case and written into the run directory. Every `$name` a case uses is an input:
//! its size and mtime go into the result key, so a changed input reruns the case.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct Case {
    pub name: String,
    /// Arguments after substitution; `$out` stays for the runner to fill in.
    pub args: Vec<String>,
    pub inputs: Vec<PathBuf>,
    /// Memory cap of its queue job.
    pub mem: String,
    pub timeout_s: u64,
    pub repeat: u32,
    pub slower_pct: f64,
    pub grow_pct: f64,
}

impl Case {
    fn template() -> Case {
        Case {
            name: String::new(),
            args: vec![],
            inputs: vec![],
            mem: "12G".into(),
            timeout_s: 1800,
            repeat: 1,
            slower_pct: 15.0,
            grow_pct: 10.0,
        }
    }

    /// Applies `t` if it is an attribute; false if it is not one.
    fn attr(&mut self, t: &str) -> Result<bool, String> {
        let pct = |v: &str| v.trim_end_matches('%').parse::<f64>().map_err(|_| format!("bad percentage {v}"));
        let Some((k, v)) = t.split_once('=') else { return Ok(false) };
        match k {
            "mem" => self.mem = v.into(),
            "timeout" => self.timeout_s = v.parse().map_err(|_| format!("bad timeout {v}"))?,
            "repeat" => self.repeat = v.parse::<u32>().map_err(|_| format!("bad repeat {v}"))?.max(1),
            "slower" => self.slower_pct = pct(v)?,
            "grow" => self.grow_pct = pct(v)?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

pub struct Sets {
    pub order: Vec<String>,
    pub sets: HashMap<String, Vec<Case>>,
}

impl Sets {
    pub fn get(&self, name: Option<&str>) -> Result<(&str, &[Case]), String> {
        let name = match name {
            Some(n) => n,
            None => self.order.first().ok_or("the case file defines no set")?,
        };
        let (name, cases) = self
            .sets
            .get_key_value(name)
            .ok_or_else(|| format!("no set [{name}]; sets: {}", self.order.join(", ")))?;
        Ok((name.as_str(), cases.as_slice()))
    }
}

pub fn parse(text: &str, root: &Path, project: &crate::project::Project) -> Result<Sets, String> {
    let mut vars: HashMap<String, PathBuf> = HashMap::new();
    let mut globs: HashMap<String, PathBuf> = HashMap::new();
    let mut sets = Sets { order: vec![], sets: HashMap::new() };
    let mut current: Option<String> = None;
    let mut defaults: HashMap<String, Case> = HashMap::new();
    for (no, raw) in text.lines().enumerate() {
        let at = |msg: String| format!("line {}: {msg}", no + 1);
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, rest)) = line.strip_prefix('[').and_then(|l| l.split_once(']')) {
            let name = name.trim().to_string();
            if sets.sets.insert(name.clone(), vec![]).is_some() {
                return Err(at(format!("set [{name}] defined twice")));
            }
            let mut d = Case::template();
            for t in split(rest).map_err(at)? {
                if !d.attr(&t).map_err(at)? {
                    return Err(at(format!("[{name}]: {t} is not an attribute")));
                }
            }
            defaults.insert(name.clone(), d);
            sets.order.push(name.clone());
            current = Some(name);
            continue;
        }
        let tokens = split(line).map_err(at)?;
        if tokens[0] == "let" {
            match tokens.as_slice() {
                [_, name, eq, value] if eq == "=" => {
                    let table = if value.contains('*') { &mut globs } else { &mut vars };
                    table.insert(name.clone(), root.join(value));
                }
                _ => return Err(at("expected `let NAME = PATH`".into())),
            }
            continue;
        }
        let set = current.as_ref().ok_or_else(|| at("a case before any [set]".into()))?;
        if tokens[0] == "include" {
            let [_, other] = tokens.as_slice() else {
                return Err(at("expected `include SET`".into()));
            };
            let cases = sets
                .sets
                .get(other)
                .ok_or_else(|| at(format!("include of [{other}], not defined above")))?
                .clone();
            sets.sets.get_mut(set).unwrap().extend(cases);
            continue;
        }
        let mut template = Case { name: tokens[0].clone(), ..defaults[set].clone() };
        let mut rest = tokens[1..].iter().peekable();
        while let Some(t) = rest.peek() {
            if !template.attr(t).map_err(at)? {
                break;
            }
            rest.next();
        }
        let words: Vec<&String> = rest.collect();
        if words.is_empty() {
            return Err(at(format!("case {} has no command", template.name)));
        }
        // A `$var` bound to a glob makes one case per match, named by the `*` in the case name.
        let over = words.iter().filter_map(|t| t.strip_prefix('$')).find(|v| globs.contains_key(*v));
        let bindings: Vec<Option<(&str, PathBuf)>> = match over {
            None => vec![None],
            Some(var) if template.name.contains('*') => glob(&globs[var]).into_iter().map(|p| Some((var, p))).collect(),
            Some(var) => return Err(at(format!("case {} runs over ${var}, a glob: its name needs a *", template.name))),
        };
        for binding in bindings {
            let mut case = template.clone();
            if let Some((_, p)) = &binding {
                let stem = p.file_stem().unwrap_or_default().to_string_lossy();
                case.name = case.name.replace('*', &stem);
            }
            for t in &words {
                match t.strip_prefix('$') {
                    Some("out") => case.args.push((*t).clone()),
                    Some(var) => {
                        let path = match &binding {
                            Some((g, p)) if *g == var => p.clone(),
                            _ => vars.get(var).cloned().ok_or_else(|| at(format!("${var} is not defined")))?,
                        };
                        case.args.push(path.to_string_lossy().into_owned());
                        case.inputs.push(path);
                    }
                    None => case.args.push((*t).clone()),
                }
            }
            let named: Vec<PathBuf> = case.inputs.iter().flat_map(|p| named_by(p, root, &project.inputs).into_iter().chain(beside(p, &project.beside))).collect();
            case.inputs.extend(named);
            let cases = sets.sets.get_mut(set).unwrap();
            if cases.iter().any(|c| c.name == case.name) {
                return Err(at(format!("case {} twice in [{set}]", case.name)));
            }
            cases.push(case);
        }
    }
    Ok(sets)
}

/// Files a program reads beside its input without being told, by gate.conf's `beside` patterns (`{stem}` the
/// input's stem): without them on another machine the program may quietly compare with nothing.
pub fn beside(input: &Path, patterns: &[String]) -> Vec<PathBuf> {
    let (Some(dir), Some(stem)) = (input.parent(), input.file_stem().and_then(|s| s.to_str())) else { return vec![] };
    if patterns.is_empty() {
        return vec![];
    }
    let patterns: Vec<String> = patterns.iter().map(|p| p.replace("{stem}", stem)).collect();
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut out: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p != input && p.is_file() && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| patterns.iter().any(|w| wild(w, n))))
        .collect();
    out.sort();
    out
}

/// What a JSON input names at gate.conf's `inputs` pointers (a string or an array of strings, relative to the
/// root): inputs as much as the JSON itself, so a changed one reruns the case and a missing one skips it.
fn named_by(input: &Path, root: &Path, pointers: &[String]) -> Vec<PathBuf> {
    if pointers.is_empty() || input.extension().is_none_or(|e| e != "json") {
        return vec![];
    }
    let Ok(text) = std::fs::read_to_string(input) else { return vec![] };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return vec![] };
    let strings = |p: &String| match v.pointer(p) {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|s| s.as_str().map(String::from)).collect(),
        _ => vec![],
    };
    // A relative path is the root's: the case's own cwd (a worktree) may not have it.
    pointers.iter().flat_map(strings).map(|p| root.join(p)).collect()
}

/// Paths matching an absolute pattern whose components may hold `*`; sorted.
fn glob(pattern: &Path) -> Vec<PathBuf> {
    let mut paths = vec![PathBuf::new()];
    for comp in pattern.components() {
        let part = comp.as_os_str().to_string_lossy();
        if !part.contains('*') {
            paths = paths.into_iter().map(|p| p.join(comp)).filter(|p| p.exists()).collect();
            continue;
        }
        let mut next = vec![];
        for dir in &paths {
            for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                if wild(&part, &e.file_name().to_string_lossy()) {
                    next.push(e.path());
                }
            }
        }
        paths = next;
    }
    paths.sort();
    paths
}

fn wild(p: &str, s: &str) -> bool {
    match p.split_once('*') {
        None => p == s,
        Some((head, tail)) => {
            let Some(s) = s.strip_prefix(head) else { return false };
            (0..=s.len()).filter(|&i| s.is_char_boundary(i)).any(|i| wild(tail, &s[i..]))
        }
    }
}

/// Whitespace-separated words; a double-quoted word may hold spaces; `#` after a space starts a comment.
fn split(line: &str) -> Result<Vec<String>, String> {
    let mut out = vec![];
    let mut chars = line.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        match chars.peek() {
            None | Some('#') => break,
            Some('"') => {
                chars.next();
                let word: String = chars.by_ref().take_while(|&c| c != '"').collect();
                out.push(word);
            }
            Some(_) => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                out.push(word);
            }
        }
    }
    if line.matches('"').count() % 2 == 1 {
        return Err("unclosed quote".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str, root: &Path) -> Result<Sets, String> {
        super::parse(text, root, &Default::default())
    }

    #[test]
    fn files_beside_an_input_go_with_it() {
        let dir = std::env::temp_dir().join(format!("gate-beside-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["s.in", "s_01.s", "s_02.s", "s.part1", "s.out", "other_01.other", "sx_01.sx"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        let patterns = ["{stem}_*.{stem}".to_string(), "{stem}.part*".to_string()];
        let got: Vec<String> = beside(&dir.join("s.in"), &patterns).iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(got, ["s.part1", "s_01.s", "s_02.s"]);
        assert!(beside(&dir.join("s.in"), &[]).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    const TEXT: &str = r#"
let a = data/test/a.csv
let b = "data/with spaces/b.csv"   # spaces
[quick]
a.json  convert --to json $a -o $out
[full] timeout=99 repeat=2
include quick
b.db mem=20G timeout=60 slower=30% import --opt mode=fast $b -o $out
"#;

    #[test]
    fn sets_vars_and_attributes() {
        let sets = parse(TEXT, Path::new("/r")).unwrap();
        let (name, quick) = sets.get(None).unwrap();
        assert_eq!(name, "quick");
        assert_eq!(quick[0].args, ["convert", "--to", "json", "/r/data/test/a.csv", "-o", "$out"]);
        assert_eq!(quick[0].inputs, [PathBuf::from("/r/data/test/a.csv")]);
        let (_, full) = sets.get(Some("full")).unwrap();
        assert_eq!(full.len(), 2);
        assert_eq!((full[1].mem.as_str(), full[1].timeout_s), ("20G", 60));
        // Set defaults apply to its own cases, not to included ones; a case's own attribute wins.
        assert_eq!((full[1].repeat, full[1].slower_pct, full[1].grow_pct), (2, 30.0, 10.0));
        assert_eq!((full[0].repeat, full[0].timeout_s), (1, 1800));
        assert!(parse("[a] bogus\n", Path::new("/")).is_err());
        assert_eq!(full[1].args[3], "/r/data/with spaces/b.csv");
    }

    #[test]
    fn a_glob_makes_a_case_per_file_and_a_json_input_names_more() {
        let root = std::env::temp_dir().join(format!("gate-glob-{}", std::process::id()));
        for day in ["d1", "d2"] {
            std::fs::create_dir_all(root.join("jobs").join(day)).unwrap();
        }
        let source = root.join("source.bin");
        std::fs::write(&source, "").unwrap();
        // Named relative to the root: not the cwd's, a worktree may not have it.
        std::fs::write(root.join("jobs/d1/move-1.json"), r#"{"input":{"base":"source.bin","extra":["/no/such.d"]}}"#).unwrap();
        std::fs::write(root.join("jobs/d2/set-2.json"), "{}").unwrap();
        std::fs::write(root.join("jobs/d2/notes.txt"), "").unwrap();
        let text = "let r = jobs/*/*.json\n[jobs]\njob-* timeout=60 apply $r\n";
        let project = crate::project::Project { inputs: vec!["/input/base".into(), "/input/extra".into()], ..Default::default() };
        let sets = super::parse(text, &root, &project).unwrap();
        let (_, cases) = sets.get(None).unwrap();
        let names: Vec<&str> = cases.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["job-move-1", "job-set-2"]);
        assert_eq!(cases[0].inputs[1..], [source, PathBuf::from("/no/such.d")]);
        assert_eq!(cases[1].inputs.len(), 1);
        assert_eq!(parse(text, &root).unwrap().get(None).unwrap().1[0].inputs.len(), 1, "no pointers: the JSON alone");
        assert!(parse("let r = jobs/*.json\n[a]\nnostar apply $r\n", &root).is_err());
        std::fs::remove_dir_all(root).unwrap();
        assert!(wild("a*b*c", "aXXbYc") && !wild("a*b", "ab_") && wild("*", ""));
    }

    #[test]
    fn errors_name_the_line() {
        let err = parse("[a]\nx convert $nope", Path::new("/")).err().unwrap();
        assert_eq!(err, "line 2: $nope is not defined");
        assert!(parse("x convert", Path::new("/")).is_err());
        assert!(parse("[a]\nx \"open", Path::new("/")).is_err());
    }
}
