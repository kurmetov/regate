//! What the gate needs to know about the project it tests: `gate.conf` in the project root (or in
//! its data dir, for a project that keeps it out of its tree), `key = value` lines, `#` comments.
//! `build` and `bin` are needed to build; every other key is optional.
//!
//! ```text
//! cases    = tests/gate.cases                  # the case file, relative to the root
//! build    = cargo build --release -p app      # run in the build worktree, split on spaces (a script for more)
//! bin      = ../target/release/app             # what the build made, relative to the build worktree
//!                                              # (../target is the gate's CARGO_TARGET_DIR)
//! env      = CARGO_PROFILE_RELEASE_LTO=false   # the build's environment, whatever the caller's says
//! tag      =                                   # names the binary in bin/ and history; default a hash of build, bin, env
//! check    = scripts/check-outputs.sh          # given a case's output files; a non-zero exit fails the case
//! stale    = is not the input it was kept of   # in a failed case's log: status stale; empty: never
//! volatile = elapsed_seconds                   # JSON keys an output's hash leaves out; empty: none
//! measures = APP_MEASURES                      # one more variable naming the measures file (REGATE_MEASURES always does)
//! beside   = {stem}_*.{stem}                   # files beside an input the program reads unasked: inputs too
//! inputs   = /input/base /input/extra          # JSON pointers in a JSON input naming more inputs (root-relative)
//! ```

use std::path::{Path, PathBuf};

pub const FILE: &str = "gate.conf";

#[derive(Clone, Debug, PartialEq)]
pub struct Project {
    pub cases: PathBuf,
    pub build: Vec<String>,
    pub bin: PathBuf,
    /// `K=V` the build runs with; part of the binary's name.
    pub env: Vec<(String, String)>,
    pub tag: Option<String>,
    /// A command given every output file of a case (absolute paths), run in the root: what a hash cannot see,
    /// an output that is well formed but empty inside.
    pub check: Vec<String>,
    /// What a case prints when its input is not the one it was kept of; empty: no such check.
    pub stale: String,
    /// Keys of a JSON output that change from run to run (a time taken): its hash is of the JSON without them.
    pub volatile: Vec<String>,
    pub measures: Option<String>,
    /// File name patterns, `{stem}` the input's stem, `*` any text.
    pub beside: Vec<String>,
    /// JSON pointers to a string or an array of strings.
    pub inputs: Vec<String>,
}

impl Default for Project {
    fn default() -> Project {
        Project {
            cases: "tests/gate.cases".into(),
            build: vec![],
            bin: PathBuf::new(),
            env: vec![],
            tag: None,
            check: vec![],
            stale: String::new(),
            volatile: vec![],
            measures: None,
            beside: vec![],
            inputs: vec![],
        }
    }
}

impl Project {
    /// `<root>/gate.conf`, else `<data>/gate.conf`, else the defaults.
    pub fn load(root: &Path) -> Result<Project, String> {
        for path in [root.join(FILE), crate::data_dir(root).join(FILE)] {
            match std::fs::read_to_string(&path) {
                Ok(t) => return Project::parse(&t).map_err(|e| format!("{}: {e}", path.display())),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Ok(Project::default())
    }

    fn parse(text: &str) -> Result<Project, String> {
        let mut p = Project::default();
        // `#` starts a comment only at a line's start or after a space: `{stem}#*` is a pattern.
        for line in text.lines().map(|l| l.split(" #").next().unwrap().trim()).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (k, v) = line.split_once('=').ok_or(format!("not key = value: {line}"))?;
            let (k, v) = (k.trim(), v.trim());
            let words: Vec<String> = v.split_whitespace().map(String::from).collect();
            match k {
                "cases" => p.cases = v.into(),
                "build" => p.build = words,
                "bin" => p.bin = v.into(),
                "tag" if !v.is_empty() => p.tag = Some(v.into()),
                "tag" => p.tag = None,
                "stale" => p.stale = v.into(),
                "volatile" => p.volatile = words,
                "measures" => p.measures = (!v.is_empty()).then(|| v.into()),
                "beside" => p.beside = words,
                "inputs" => p.inputs = words,
                "env" => {
                    p.env = words.iter().map(|w| w.split_once('=').map(|(k, v)| (k.to_string(), v.to_string())).ok_or(format!("env: {w} is not K=V"))).collect::<Result<_, _>>()?
                }
                "check" => p.check = words,
                // A key of another version of regate must not stop this one.
                _ => eprintln!("regate: gate.conf: unknown key {k}, ignored"),
            }
        }
        if !p.build.is_empty() && p.bin.file_name().is_none() {
            return Err("bin names no file".into());
        }
        if let Some(t) = p.tag.as_ref().filter(|t| t.contains(['.', '/'])) {
            return Err(format!("tag {t}: no . or / in it"));
        }
        Ok(p)
    }

    /// What the binary is called under bin/<tree>.<tag>/: the build's own name for it.
    pub fn bin_name(&self) -> &std::ffi::OsStr {
        self.bin.file_name().unwrap_or("bin".as_ref())
    }

    /// The binary's name after its tree: another build of the same tree is another binary.
    pub fn tag(&self) -> String {
        // Without env the hash is what it was before env existed, so binaries keep their names.
        let env: String = self.env.iter().map(|(k, v)| format!("\n{k}={v}")).collect();
        self.tag.clone().unwrap_or_else(|| crate::md5_str(&format!("{:?}\n{}{env}", self.build, self.bin.display()))[..8].to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_override_the_defaults() {
        assert_eq!(Project::parse("# nothing\n").unwrap(), Project::default());
        let p = Project::parse("cases = gate/cases  # mine\nbuild = make  app\nbin = out/app\nenv = A=1 B=x=y\nbeside = {stem}#*\n").unwrap();
        assert_eq!((p.cases.to_str(), p.build.join(" "), p.bin_name().to_str()), (Some("gate/cases"), "make app".into(), Some("app")));
        assert_eq!(p.env, [("A".into(), "1".into()), ("B".into(), "x=y".into())]);
        assert_eq!(p.beside, ["{stem}#*"]);
        assert!(p.check.is_empty() && p.stale.is_empty() && p.volatile.is_empty() && p.measures.is_none());
        let other = Project { env: vec![], ..p.clone() };
        assert_ne!(p.tag(), other.tag(), "another environment is another binary");
        assert_eq!(Project::parse("tag = false-16-false").unwrap().tag(), "false-16-false");
        assert!(Project::parse("bogus = 1").is_ok(), "an unknown key is ignored");
        for bad in ["build = make\nbin = /", "env = A", "tag = a.b"] {
            assert!(Project::parse(bad).is_err(), "{bad}");
        }
    }
}
