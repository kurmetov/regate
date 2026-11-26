//! `regate guard`: a Claude Code PreToolUse hook for Bash. Reads the hook's JSON on stdin and
//! refuses a command that breaks this machine's rules, saying how to do it right. The rules are for
//! many agent sessions on one box and one checkout: each one is on when guard.conf sets its key.
//!
//! guard.conf (`REGATE_GUARD`, else `$XDG_CONFIG_HOME/regate/guard.conf`, else `~/.config/regate/guard.conf`;
//! without it the hook refuses nothing):
//!
//! ```text
//! root       = /home/me/dev/app            # the checkout sessions share: no add -A, stash, reset, checkout of a
//!                                          # branch, clean or --amend in it (its worktrees are free); cargo fmt anywhere
//! commit     = git commit -m MSG -- <your files>   # how a session commits only its own paths (shown when refused)
//! worktrees  = .claude/worktrees/<name>    # where worktrees go, under root (shown when refused)
//! build      = regate queue --kind build --   # bare cargo build/test/clippy/check/run is sent here
//! heavy      = regate queue --kind heavy -m <cap> --   # systemd-run and headless Chrome are sent here
//! heavy_programs = python* node java       # ...and these programs given a file over heavy_size
//! heavy_size = 100M
//! dirs       = /tmp/scratch ~/dev/*        # mkdir only under root and these; `X/*`: inside a repo already in X
//! push       = origin                      # the only remotes a push may go to
//! merge_set  = refactor                    # `git merge B` into merge_into in root needs B's tree green on this set
//! merge_into = main
//! merge_paths = src Cargo.toml Cargo.lock  # a merge changing none of these needs no gate; empty: any change does
//! ```
//!
//! Force push, `push --no-verify`, and kill by name or pattern are refused whenever guard.conf exists.

use std::path::{Path, PathBuf};

#[derive(Debug, Default)]
pub struct Conf {
    root: Option<PathBuf>,
    commit: String,
    worktrees: String,
    build: Option<String>,
    heavy: Option<String>,
    heavy_programs: Vec<String>,
    heavy_mb: u64,
    dirs: Option<Vec<String>>,
    push: Option<Vec<String>>,
    merge_set: Option<String>,
    merge_into: String,
    merge_paths: Vec<String>,
}

impl Conf {
    fn path() -> Option<PathBuf> {
        crate::env("GUARD").map(PathBuf::from).or_else(|| {
            let config = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")))?;
            Some(config.join("regate/guard.conf"))
        })
    }

    pub fn parse(text: &str) -> Result<Conf, String> {
        let mut c = Conf {
            commit: "git commit -m MSG -- <your files> (new files: `git add <f> && git commit -m MSG -- <f>`, one command)".into(),
            worktrees: ".claude/worktrees/<name>".into(),
            heavy_mb: 100,
            merge_into: "main".into(),
            ..Default::default()
        };
        for line in text.lines().map(|l| l.split(" #").next().unwrap().trim()).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (k, v) = line.split_once('=').ok_or(format!("not key = value: {line}"))?;
            let (k, v) = (k.trim(), v.trim().to_string());
            let words = || v.split_whitespace().map(String::from).collect::<Vec<_>>();
            match k {
                "root" => c.root = Some(PathBuf::from(v.trim_end_matches('/'))),
                "commit" => c.commit = v,
                "worktrees" => c.worktrees = v,
                "build" => c.build = Some(v),
                "heavy" => c.heavy = Some(v),
                "heavy_programs" => c.heavy_programs = words(),
                "heavy_size" => c.heavy_mb = crate::queue::parse_mem(&v).ok_or(format!("heavy_size {v}: not a size like 100M"))?,
                "dirs" => c.dirs = Some(words()),
                "push" => c.push = Some(words()),
                "merge_set" => c.merge_set = Some(v),
                "merge_into" => c.merge_into = v,
                "merge_paths" => c.merge_paths = words(),
                _ => return Err(format!("unknown key {k}")),
            }
        }
        Ok(c)
    }
}

/// The hook: stdin JSON in, a deny decision on stdout (or nothing), exit 0 either way.
pub fn main() -> i32 {
    let Some(path) = Conf::path() else { return 0 };
    let conf = match std::fs::read_to_string(&path) {
        Ok(t) => Conf::parse(&t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(e) => Err(e.to_string()),
    };
    // A broken config is said, not taken for no rules: the hook's stderr reaches the session.
    let conf = match conf {
        Ok(c) => c,
        Err(e) => {
            eprintln!("regate guard: {}: {e}", path.display());
            return 0;
        }
    };
    let mut input = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
        return 0;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&input) else { return 0 };
    let Some(cmd) = v.pointer("/tool_input/command").and_then(|c| c.as_str()) else { return 0 };
    let cwd = v.get("cwd").and_then(|c| c.as_str()).map(PathBuf::from).or_else(|| std::env::current_dir().ok()).unwrap_or_default();
    if let Some(why) = check(cmd, &cwd, &conf, &|p| std::fs::metadata(p).map(|m| m.len()).ok()) {
        let out = serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": why}});
        println!("{out}");
    }
    0
}

/// Words of one simple command, quotes resolved; a command list splits at ; & | && || and newlines.
/// A here-document's body is data, not commands. `$(...)` stays one word; check() looks inside it.
fn split(cmd: &str) -> Vec<Vec<String>> {
    let (mut cmds, mut words, mut w) = (vec![], vec![], None::<String>);
    let mut heredocs: Vec<(String, bool)> = vec![];
    let mut it = cmd.chars().peekable();
    let end_word = |words: &mut Vec<String>, w: &mut Option<String>| {
        if let Some(s) = w.take() {
            words.push(s)
        }
    };
    while let Some(c) = it.next() {
        match c {
            '\'' => {
                let s = w.get_or_insert_with(String::new);
                for c in it.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    s.push(c);
                }
            }
            '"' => {
                let s = w.get_or_insert_with(String::new);
                while let Some(c) = it.next() {
                    match c {
                        '"' => break,
                        '\\' => s.extend(it.next()),
                        c => s.push(c),
                    }
                }
            }
            '\\' => {
                if let Some(c) = it.next().filter(|&c| c != '\n') {
                    w.get_or_insert_with(String::new).push(c);
                }
            }
            '$' if it.peek() == Some(&'(') => {
                let s = w.get_or_insert_with(String::new);
                s.push('$');
                let mut depth = 0;
                for c in it.by_ref() {
                    s.push(c);
                    depth += (c == '(') as i32 - (c == ')') as i32;
                    if depth == 0 {
                        break;
                    }
                }
            }
            '<' if it.peek() == Some(&'<') => {
                it.next();
                if it.peek() == Some(&'<') {
                    it.next();
                    continue;
                }
                let dash = it.peek() == Some(&'-');
                if dash {
                    it.next();
                }
                while it.peek().is_some_and(|c| *c == ' ' || *c == '\t') {
                    it.next();
                }
                let mut d = String::new();
                while let Some(&c) = it.peek() {
                    if c.is_whitespace() || matches!(c, ';' | '&' | '|' | ')') {
                        break;
                    }
                    it.next();
                    if !matches!(c, '\'' | '"' | '\\') {
                        d.push(c);
                    }
                }
                heredocs.push((d, dash));
            }
            '#' if w.is_none() => {
                while it.peek().is_some_and(|&c| c != '\n') {
                    it.next();
                }
            }
            // 2>&1, &> and >& redirect, they do not end the command.
            '&' if w.as_ref().is_some_and(|s| s.ends_with('>')) || it.peek() == Some(&'>') => w.get_or_insert_with(String::new).push(c),
            ';' | '&' | '|' | '\n' | '(' | ')' => {
                end_word(&mut words, &mut w);
                if !words.is_empty() {
                    cmds.push(std::mem::take(&mut words));
                }
                if c == '\n' {
                    for (d, dash) in heredocs.drain(..) {
                        let mut line = String::new();
                        loop {
                            match it.next() {
                                None => break,
                                Some('\n') => {
                                    let l = if dash { line.trim_start_matches('\t') } else { line.as_str() };
                                    if l == d {
                                        break;
                                    }
                                    line.clear();
                                }
                                Some(c) => line.push(c),
                            }
                        }
                    }
                }
            }
            c if c.is_whitespace() => end_word(&mut words, &mut w),
            c => w.get_or_insert_with(String::new).push(c),
        }
    }
    end_word(&mut words, &mut w);
    if !words.is_empty() {
        cmds.push(words);
    }
    cmds
}

/// Every simple command, those inside `$(...)` too (not a `$(cat <<EOF ...)` message).
fn commands(cmd: &str) -> Vec<Vec<String>> {
    let mut out = vec![];
    for words in split(cmd) {
        for w in &words {
            let mut rest = w.as_str();
            while let Some(i) = rest.find("$(") {
                let inner = &rest[i + 2..];
                let end = inner.rfind(')').unwrap_or(inner.len());
                if !inner.trim_start().starts_with("cat") {
                    out.extend(commands(&inner[..end]));
                }
                rest = &inner[end..];
            }
        }
        out.push(words);
    }
    out
}

/// The program a command runs once `VAR=x`, `env`, `time`, `nice` and `timeout N` are passed, and
/// whether `GIT_INDEX_FILE=` was set before it (a private index).
fn program(words: &[String]) -> (&[String], bool) {
    let mut i = 0;
    let private = words.iter().take_while(|w| w.contains('=') || matches!(w.as_str(), "env")).any(|w| w.starts_with("GIT_INDEX_FILE="));
    while i < words.len() {
        let w = words[i].as_str();
        if w.contains('=') && !w.starts_with('-') && !w.starts_with('=') {
            i += 1;
        } else if matches!(w, "env" | "time" | "nice" | "nohup" | "exec" | "command" | "stdbuf" | "ionice") {
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                i += 1;
            }
        } else if w == "timeout" {
            i += 1;
            while i < words.len() && words[i].starts_with('-') {
                i += 1;
            }
            i += 1;
        } else {
            break;
        }
    }
    (&words[i.min(words.len())..], private)
}

fn base(w: &str) -> &str {
    w.rsplit('/').next().unwrap_or(w)
}

fn resolve(dir: &Path, p: &str) -> PathBuf {
    let p = match p.strip_prefix("~") {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{}{rest}", std::env::var("HOME").unwrap_or_default()),
        _ => p.to_string(),
    };
    let mut out = PathBuf::new();
    for c in dir.join(p).components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

/// In the shared root itself, not in a worktree or another repository under it.
fn in_root(dir: &Path, conf: &Conf) -> bool {
    let Some(root) = &conf.root else { return false };
    dir.starts_with(root) && !dir.ancestors().take_while(|a| a != root).any(|a| a.join(".git").exists())
}

/// Why `cmd`, run in `cwd`, breaks a rule; `size` gives a file's length.
pub fn check(cmd: &str, cwd: &Path, conf: &Conf, size: &dyn Fn(&Path) -> Option<u64>) -> Option<String> {
    let mut dir = cwd.to_path_buf();
    let all = commands(cmd);
    let commits = all.iter().any(|w| program(w).0.iter().take(4).any(|a| a == "commit"));
    for words in &all {
        let (words, private) = program(words);
        let Some(prog) = words.first() else { continue };
        let args = &words[1..];
        let has = |f: &str| args.iter().any(|a| a == f);
        match base(prog) {
            "cd" | "pushd" => {
                // A directory from a variable is not known here: taken as not the root, so its git is not judged.
                if let Some(a) = args.iter().find(|a| !a.starts_with('-')) {
                    dir = if a.contains('$') || a.contains('`') { PathBuf::from("/unknown") } else { resolve(&dir, a) };
                }
            }
            "cargo" => {
                let sub = args.iter().find(|a| !a.starts_with('-') && !a.starts_with('+')).map(String::as_str);
                match sub {
                    Some("fmt") if conf.root.is_some() => return Some(FMT.into()),
                    None | Some("fmt" | "metadata" | "tree" | "locate-project" | "pkgid" | "search" | "info" | "help" | "version" | "deny" | "machete") => {} // deny, machete: they read the lockfile and sources, nothing is compiled
                    Some(_) if has("--version") || has("-V") => {}
                    Some(s) => {
                        if let Some(via) = &conf.build {
                            return Some(format!(
                                "bare `cargo {s}`: every cargo build/test/clippy/check/run goes through the machine-wide queue, \
                                 `{via} cargo {s} -p <crate> ...` (cold or parallel builds side by side run the box out of \
                                 memory). Check `regate queue status` if it waits."
                            ));
                        }
                    }
                }
            }
            // A wrapped cargo: fmt is the one subcommand the queue does not make safe.
            w if conf.root.is_some() && !matches!(w, "echo" | "printf") && args.iter().position(|a| base(a) == "cargo").is_some_and(|c| args.get(c + 1).is_some_and(|s| s == "fmt")) => {
                return Some(FMT.into());
            }
            "git" => {
                let mut gdir = dir.clone();
                let mut i = 0;
                while i < args.len() && args[i].starts_with('-') {
                    if args[i] == "-C" && i + 1 < args.len() {
                        gdir = resolve(&gdir, &args[i + 1]);
                        i += 1;
                    } else if args[i] == "-c" {
                        i += 1;
                    }
                    i += 1;
                }
                if let Some(why) = git(&args[i.min(args.len())..], &gdir, private || commits, conf) {
                    return Some(why);
                }
            }
            "pkill" | "killall" => {
                return Some(
                    "kill by name or pattern: `pkill -f` matches this shell's own command line (exit 144) and \
                     `pkill -x`/`killall` hits every process of that name, the user's servers too. Stop exactly the PID \
                     you started (`... & echo $! > <dir>/x.pid`, then `kill $(cat <dir>/x.pid)`)."
                        .into(),
                );
            }
            "pgrep" if has("-f") && all.iter().flatten().any(|w| w == "kill" || w.contains("kill ")) => {
                let pat = args.iter().rfind(|a| !a.starts_with('-'));
                if !pat.is_some_and(|p| p.starts_with("^/")) {
                    return Some(
                        "`pgrep -f` feeding a kill: the pattern matches this shell too. Kill the PID you recorded, or \
                         anchor the pattern to the program's path (`pgrep -f '^/path/to/prog'`)."
                            .into(),
                    );
                }
            }
            "mkdir" => {
                for a in args.iter().filter(|a| !a.starts_with('-')) {
                    let p = resolve(&dir, a);
                    if !allowed_dir(&p, conf) {
                        let inside = conf.root.as_ref().map(|r| format!(" Keep work under {}, small scratch in the session scratchpad.", r.display())).unwrap_or_default();
                        return Some(format!(
                            "mkdir {}: no directories outside the project (stray directories pile up in the user's home).{inside} \
                             If it must live elsewhere, ask the user first.",
                            p.display()
                        ));
                    }
                }
            }
            "systemd-run" if conf.heavy.is_some() => {
                return Some(format!(
                    "direct `systemd-run`: heavy jobs go through `{}` (the queue counts every cap against free memory; \
                     scopes beside it are what it cannot see).",
                    conf.heavy.as_deref().unwrap_or_default()
                ));
            }
            p => {
                if let Some(why) = heavy(p, args, &dir, conf, size) {
                    return Some(why);
                }
            }
        }
    }
    None
}

const FMT: &str = "`cargo fmt` formats the whole workspace while other sessions edit it. Format \
                   only your files: `rustfmt --edition 2024 <files>`, then check it touched nothing else \
                   (a crate root formats its whole crate).";

/// Under root, or under guard.conf's `dirs`; `X/*` there: inside a git repository already in X, not X/<repo> itself.
fn allowed_dir(p: &Path, conf: &Conf) -> bool {
    let Some(dirs) = &conf.dirs else { return true };
    if conf.root.as_ref().is_some_and(|r| p.starts_with(r)) {
        return true;
    }
    dirs.iter().any(|d| {
        let d = resolve(Path::new("/"), d);
        match d.file_name().is_some_and(|n| n == "*") {
            false => p.starts_with(&d),
            true => {
                let top = d.parent().unwrap_or(Path::new("/"));
                p.strip_prefix(top).ok().and_then(|r| r.components().next()).is_some_and(|c| {
                    let repo = top.join(c);
                    p != repo && repo.join(".git").exists()
                })
            }
        }
    })
}

/// `committing`: a private index is in use, or the same command commits what it adds.
fn git(args: &[String], dir: &Path, committing: bool, conf: &Conf) -> Option<String> {
    let (sub, rest) = args.split_first()?;
    let flag = |f: &str| rest.iter().any(|a| a == f);
    // Short flags bundled: -am has a.
    let short = |c: char| rest.iter().any(|a| a.starts_with('-') && !a.starts_with("--") && a[1..].contains(c));
    let paths: Vec<&String> = rest.iter().skip_while(|a| *a != "--").skip(1).collect();
    let spec: Vec<&String> = if paths.is_empty() { rest.iter().filter(|a| !a.starts_with('-')).collect() } else { paths.clone() };
    if sub == "push" {
        if flag("--force") || flag("-f") || rest.iter().any(|a| a.starts_with("--force-with-lease")) || rest.iter().any(|a| a.starts_with('+')) {
            return Some("force push: never, on any remote. Merge or add a commit instead.".into());
        }
        if flag("--no-verify") {
            return Some("`git push --no-verify` skips the remote's pre-push hook: never.".into());
        }
        if let (Some(allowed), Some(remote)) = (&conf.push, rest.iter().find(|a| !a.starts_with('-')))
            && !allowed.contains(remote)
        {
            return Some(format!("push to `{remote}`: push only to {}. Anything else only when the user asks.", allowed.join(", ")));
        }
        return None;
    }
    if !in_root(dir, conf) {
        return None;
    }
    let root = conf.root.as_ref()?.display().to_string();
    if sub == "commit" && flag("--amend") {
        return Some("`git commit --amend` in the shared checkout rewrites what other sessions already built on. Add a new commit.".into());
    }
    if sub == "merge" {
        return merge_gate(rest, dir, conf);
    }
    // `git checkout --theirs f`, `git checkout HEAD f`: files by name, not a branch.
    let files = flag("--ours") || flag("--theirs") || rest.iter().filter(|a| !a.starts_with('-')).any(|a| a.contains('/') && dir.join(a).exists());
    let shared = "the root tree and its index are shared by every session";
    let commit_how = format!("commit only your paths: `{}`", conf.commit);
    let worktree = format!("{root}/{}", conf.worktrees);
    match sub.as_str() {
        "add" if flag("-A") || flag("--all") || flag(".") || flag(":/") || flag("-u") || flag("--update") || short('A') || short('u') => {
            Some(format!("`git add` of everything: {shared}; it would take their work into your commit. Add your files by name, or {commit_how}."))
        }
        "add" if !committing => Some(format!(
            "`git add` in the root leaves files in the shared index, where another session's plain `git commit` takes them. {commit_how}."
        )),
        "commit" if flag("--all") || short('a') => Some(format!("`git commit -a`: {shared}. {commit_how}.")),
        "stash" if !matches!(rest.first().map(String::as_str), Some("list" | "show")) && paths.is_empty() => {
            Some(format!("`git stash` in the root takes every session's uncommitted work with it: {shared}. Work in a worktree ({worktree}) if you need a clean tree."))
        }
        "reset" if flag("--hard") || flag("--merge") || flag("--keep") || flag("--soft") || paths.is_empty() => Some(format!(
            "`git reset` without paths in the root moves the shared index or HEAD: {shared}. Only `git reset -q -- <your paths>` \
             (index only) after a private-index commit."
        )),
        "checkout" | "restore" if spec.iter().any(|p| matches!(p.as_str(), "." | ":/" | "*")) => {
            Some(format!("`git {sub} .` throws away every session's uncommitted edits: {shared}. Restore your own files by name."))
        }
        "checkout" | "switch" if sub == "switch" || (paths.is_empty() && !files) || flag("-f") || flag("--force") => Some(format!(
            "`git {sub}` of a branch in the root switches it under every session. Use a worktree: `git worktree add {worktree} <branch>`."
        )),
        "clean" => Some(format!("`git clean` in the root deletes other sessions' new files: {shared}.")),
        "worktree" if rest.first().is_some_and(|s| s == "add") => {
            let p = rest.iter().skip(1).find(|a| !a.starts_with('-'))?;
            let p = resolve(dir, p);
            (!p.starts_with(&root)).then(|| format!("worktree at {}: worktrees go under the root, {worktree}.", p.display()))
        }
        _ => None,
    }
}

/// `git merge X` into merge_into in the root: X's tree must have passed every case of merge_set this machine
/// can run. A gate result that is only text in a commit message can claim "0 differ" after a third of the set.
fn merge_gate(rest: &[String], dir: &Path, conf: &Conf) -> Option<String> {
    let set = conf.merge_set.as_deref()?;
    let mut refs = vec![];
    let mut i = 0;
    while i < rest.len() {
        let a = &rest[i];
        match a.as_str() {
            "--abort" | "--continue" | "--quit" | "--skip" => return None,
            "-m" | "-F" | "-s" | "-X" | "--message" | "--file" | "--strategy" | "--strategy-option" | "--into-name" => i += 1,
            s if s.starts_with('-') => {}
            _ => refs.push(a.clone()),
        }
        i += 1;
    }
    let git = |args: &[&str]| crate::sh_out(std::process::Command::new("git").arg("-C").arg(dir).args(args));
    if git(&["symbolic-ref", "--short", "HEAD"]).ok()? != conf.merge_into {
        return None;
    }
    let root = conf.root.as_deref()?;
    let project = crate::project::Project::load(root).ok()?;
    let sets = std::fs::read_to_string(root.join(&project.cases)).map_err(|e| e.to_string()).and_then(|t| crate::cases::parse(&t, root, &project)).ok()?;
    let cases = sets.get(Some(set)).ok()?.1;
    let hist = crate::history::load(&crate::data_dir(root).join("history.jsonl"));
    for r in refs {
        // A remote's branch (a pull) was gated where it was made.
        if git(&["rev-parse", "--symbolic-full-name", &r]).is_ok_and(|f| f.starts_with("refs/remotes/")) {
            continue;
        }
        let Ok(tree) = git(&["rev-parse", &format!("{r}^{{tree}}")]) else { continue };
        let range = format!("HEAD...{r}");
        let mut diff = vec!["diff", "--quiet", range.as_str(), "--"];
        diff.extend(conf.merge_paths.iter().map(String::as_str));
        if git(&diff).is_ok() {
            continue;
        }
        let bins: Vec<String> = hist.iter().filter(|h| h.kind == "build" && h.status == "ok" && h.rev.split('.').next() == Some(tree.as_str())).map(|h| h.bin.clone()).collect();
        if let Err(why) = ungated(&hist, cases, &bins) {
            let outside = if conf.merge_paths.is_empty() { String::new() } else { format!(" A change outside {} needs no gate.", conf.merge_paths.join(" ")) };
            return Some(format!(
                "merge of `{r}` into {}: its tree {} has no green `regate diff {} tree {set}` ({why}). Run it in the \
                 branch's worktree on exactly what you commit, then merge.{outside}",
                conf.merge_into,
                &tree[..12],
                conf.merge_into,
            ));
        }
    }
    None
}

/// Ok when one of `bins` (binaries built of the tree) passed every case this machine has the inputs of: ran ok,
/// no reference held worse than before. Else what is missing, for the bin that came nearest.
fn ungated(hist: &[crate::history::Rec], cases: &[crate::cases::Case], bins: &[String]) -> Result<(), String> {
    use crate::history::{accuracy, baseline, worse};
    if bins.is_empty() {
        return Err("never built by regate".into());
    }
    let mut best: Option<Vec<String>> = None;
    for bin in bins {
        let mut bad = vec![];
        for c in cases.iter().filter(|c| c.inputs.iter().all(|p| p.exists())) {
            match hist.iter().rev().find(|h| h.kind == "case" && h.name == c.name && h.bin == *bin) {
                None => bad.push(format!("{} not run", c.name)),
                Some(h) if h.status != "ok" => bad.push(format!("{} {}", c.name, h.status)),
                Some(h) if baseline(hist, h).is_some_and(|b| worse(&accuracy(&h.measures, &b.measures))) => bad.push(format!("{} less accurate", c.name)),
                Some(_) => {}
            }
        }
        if bad.is_empty() {
            return Ok(());
        }
        if best.as_ref().is_none_or(|b| bad.len() < b.len()) {
            best = Some(bad);
        }
    }
    let bad = best.expect("bins is not empty");
    let shown: Vec<&str> = bad.iter().take(4).map(String::as_str).collect();
    Err(format!("{} of {} cases: {}{}", bad.len(), cases.len(), shown.join(", "), if bad.len() > 4 { ", ..." } else { "" }))
}

/// A run that needs the heavy queue: headless Chrome, or one of heavy_programs given a file over heavy_size.
fn heavy(prog: &str, args: &[String], dir: &Path, conf: &Conf, size: &dyn Fn(&Path) -> Option<u64>) -> Option<String> {
    let via = conf.heavy.as_deref()?;
    let help = args.iter().any(|a| matches!(a.as_str(), "--help" | "-h" | "--version" | "-V" | "help"));
    if help {
        return None;
    }
    let chrome = matches!(prog, "chrome" | "chromium" | "chromium-browser" | "google-chrome" | "google-chrome-stable" | "chrome-headless-shell")
        && args.iter().any(|a| a.starts_with("--headless"));
    if chrome {
        return Some(format!("`{prog}` outside the queue: headless Chrome counts as heavy even on a small page. `{via} {prog} ...`, one at a time, closed when done."));
    }
    let listed = |p: &String| p.strip_suffix('*').map_or(p == prog, |head| prog.starts_with(head));
    if !conf.heavy_programs.iter().any(listed) {
        return None;
    }
    for a in args {
        let v = a.split_once('=').map_or(a.as_str(), |(_, v)| v);
        let v = v.split_once(':').map_or(v, |(f, _)| f); // file.pdf:N
        if v.starts_with('-') || v.is_empty() {
            continue;
        }
        let p = resolve(dir, v);
        if let Some(n) = size(&p).filter(|&n| n > conf.heavy_mb << 20) {
            return Some(format!(
                "`{prog}` on {} ({} MB) outside the queue: runs over {} MB inputs go through `{via} {prog} ...`; \
                 size the cap from the peaks `regate queue status` shows.",
                p.display(),
                n >> 20,
                conf.heavy_mb
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A machine: T/app the shared root, T/scratch a scratchpad, T/dev/tool a repo of its own.
    fn machine() -> (PathBuf, Conf) {
        let t = std::env::temp_dir().join(format!("gate-guard-{}", std::process::id()));
        for d in ["app/docs/site", "app/wt/x", "scratch", "dev/tool/.git"] {
            std::fs::create_dir_all(t.join(d)).unwrap();
        }
        std::fs::write(t.join("app/docs/site/SUMMARY.md"), "").unwrap();
        std::fs::write(t.join("app/wt/x/.git"), "gitdir: elsewhere").unwrap();
        let conf = format!(
            "root = {t}/app\nbuild = scripts/build.sh  # mine\nheavy = scripts/heavy.sh -m <cap>\nheavy_programs = app* python* node\n\
             dirs = {t}/scratch {t}/dev/*\npush = origin\n",
            t = t.display()
        );
        (t, Conf::parse(&conf).unwrap())
    }

    fn why(cmd: &str) -> Option<String> {
        let (t, conf) = machine();
        check(&cmd.replace("$T", &t.display().to_string()), &t.join("app"), &conf, &|p| p.to_str().is_some_and(|s| s.ends_with("big.dat")).then_some(300 << 20))
    }

    #[test]
    fn conf_keys_and_no_rules_without_them() {
        let c = Conf::parse("# all defaults\n").unwrap();
        assert!(c.root.is_none() && c.build.is_none() && c.heavy_mb == 100 && c.merge_into == "main");
        assert!(Conf::parse("bogus = 1").is_err() && Conf::parse("heavy_size = lots").is_err());
        let anywhere = Path::new("/");
        for c in ["cargo build", "git add -A", "git stash", "mkdir /x", "systemd-run x", "google-chrome --headless", "git push other main"] {
            assert!(check(c, anywhere, &Conf::parse("").unwrap(), &|_| Some(1 << 40)).is_none(), "{c}");
        }
        assert!(check("git push -f origin main", anywhere, &Conf::parse("").unwrap(), &|_| None).is_some());
        let (_, conf) = machine();
        assert!(why("cargo build").unwrap().contains("`scripts/build.sh cargo build"), "the conf's own wrapper is named");
        assert!(check("git add -A", Path::new("/elsewhere"), &conf, &|_| None).is_none(), "outside the root git is not judged");
    }

    #[test]
    fn merge_needs_every_case_green() {
        use crate::history::Rec;
        let case = |n: &str| crate::cases::Case { name: n.into(), args: vec![], inputs: vec![], mem: "1G".into(), timeout_s: 1, repeat: 1, slower_pct: 15.0, grow_pct: 10.0 };
        let rec = |n: &str, bin: &str, status: &str, m: f64| Rec {
            kind: "case".into(),
            name: n.into(),
            bin: bin.into(),
            status: status.into(),
            measures: [("err".to_string(), m)].into(),
            ..Default::default()
        };
        let cases = [case("a"), case("b")];
        let bins = ["new".to_string()];
        let old = [rec("a", "old", "ok", 1.0), rec("b", "old", "ok", 1.0)];
        let with = |more: &[Rec]| old.iter().chain(more).cloned().collect::<Vec<_>>();
        assert!(ungated(&with(&[rec("a", "new", "ok", 1.0), rec("b", "new", "ok", 0.5)]), &cases, &bins).is_ok());
        assert!(ungated(&with(&[rec("a", "new", "ok", 1.0)]), &cases, &bins).unwrap_err().contains("b not run"));
        assert!(ungated(&with(&[rec("a", "new", "ok", 1.0), rec("b", "new", "exit 101", 1.0)]), &cases, &bins).unwrap_err().contains("b exit 101"));
        assert!(ungated(&with(&[rec("a", "new", "ok", 2.0), rec("b", "new", "ok", 1.0)]), &cases, &bins).unwrap_err().contains("a less accurate"));
        assert!(ungated(&old, &cases, &[]).is_err());
    }

    #[test]
    fn words_and_commands() {
        assert_eq!(split("a 'b c' \"d\\\"e\" && f|g; h # x"), vec![vec!["a", "b c", "d\"e"], vec!["f"], vec!["g"], vec!["h"]]);
        assert_eq!(split("echo $(git add -A) x"), vec![vec!["echo", "$(git add -A)", "x"]]);
    }

    #[test]
    fn refuses() {
        for c in [
            "cargo build --release -p core",
            "cd crates/x && cargo test",
            "timeout 600 cargo clippy",
            "scripts/build.sh cargo fmt --all",
            "cargo fmt",
            "git add -A",
            "git -C $T/app stash",
            "git commit -am 'x'",
            "git checkout .",
            "git checkout main",
            "git checkout -q --detach main",
            "git reset --hard HEAD",
            "git reset",
            "git push --force origin main",
            "git push platform main",
            "git commit --amend --no-edit",
            "pkill -f app-server",
            "for p in $(pgrep -f 'python x'); do kill $p; done",
            "mkdir -p ~/stray-out",
            "mkdir $T/dev/new-thing",
            "systemd-run --user --scope -p MemoryMax=12G x",
            "target/release/app convert data/test/big.dat --to json",
            "google-chrome --headless=new --screenshot x",
            "git worktree add ../wt main",
            "git add src/a.rs",
            "echo $(cargo build)",
            "cd docs/site && git stash",
        ] {
            assert!(why(c).is_some(), "should refuse: {c}");
        }
    }

    #[test]
    fn lets_through() {
        for c in [
            "scripts/build.sh cargo build --release -p core",
            "scripts/heavy.sh -m 12G target/release/app convert data/test/big.dat",
            "cargo --version",
            "cargo metadata --format-version 1",
            "cargo deny check licenses",
            "cargo machete crates/core",
            "git commit -m 'git add -A is refused' -- a.rs b.rs",
            "git reset -q -- a.rs",
            "git checkout -- crates/x/src/lib.rs",
            "git status && git log -1",
            "git push origin main",
            "git stash list",
            "git checkout --theirs web/docs.json",
            "git checkout docs/site/SUMMARY.md",
            "git stash push -q -m x -- web/docs.json",
            "npm run x 2>&1 | tail -1 && git status",
            "cd $T/dev/tool && git stash",
            "cd wt/x && git stash && git checkout main",
            "cd $R && git add -A && git commit -qm init",
            "mkdir -p data/out/x $T/scratch/s/y",
            "mkdir -p $T/dev/tool/src/sub",
            "target/release/app convert data/test/small.dat",
            "app-server --help",
            "node scripts/shots.mjs target/debug/big.dat.bin",
            "cp data/test/big.dat /dev/null",
            "kill 12345",
            "pgrep -af 'cargo build|rustc'",
            "rustfmt --edition 2024 src/a.rs",
            "echo 'cargo build' > notes.txt",
            "echo cargo fmt",
            "cat > x.sh <<'EOF'\ncargo build\ngit add -A\nEOF\nchmod +x x.sh",
            "git add new.rs && git commit -m x -- new.rs",
            "GIT_INDEX_FILE=/tmp/i git add a.rs",
            "git commit -m \"$(cat <<'EOF'\nfix: no git add -A\nEOF\n)\" -- a.rs",
        ] {
            assert!(why(c).is_none(), "should let through: {c} -> {:?}", why(c));
        }
    }
}
