<p align="center">
  <img src="assets/logo.svg" alt="regate" width="220">
</p>

<p align="center">
  <b>One Linux box, a dozen coding agents, no OOM.</b><br>
  A memory-aware job queue, a regression gate and a Claude Code guard for a machine many sessions share.
</p>

<p align="center">
  <a href="https://github.com/kurmetov/regate/actions/workflows/ci.yml"><img alt="CI" src="https://github.com/kurmetov/regate/actions/workflows/ci.yml/badge.svg"></a>
  <img alt="License: MIT OR Apache-2.0" src="https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-555">
  <img alt="Platform: Linux" src="https://img.shields.io/badge/platform-Linux%20%7C%20Windows%20(WSL2)-555">
  <img alt="Rust 2024" src="https://img.shields.io/badge/rust-2024%20edition-555">
  <img alt="Claude Code plugin" src="https://img.shields.io/badge/Claude%20Code-plugin-555">
</p>

---

A regression gate and a memory-aware job queue for one Linux machine that many sessions share
(people, CI jobs, or a dozen coding agents at once).

* **Gate.** Builds a project once per git tree, runs a fixed set of cases, keeps every result
  in a history file, and judges each run against the binaries before it: crash, output changed,
  slower (CPU, not wall time), more memory, a reference measure held worse.
* **Queue.** Every build and heavy run waits for room by the machine's *available* memory, then runs
  in a systemd scope capped at its memory (no swap) and CPU. Jobs that have run before reserve
  what they really peaked at, not their cap.
* **Guard.** A Claude Code `PreToolUse` hook that refuses the shell commands that hurt a shared
  box or a shared checkout (bare `cargo build`, `git add -A`, `git stash` in the shared root,
  merging into main without a green gate, ...) and says what to run instead.

Linux (Windows through WSL2, see [Platforms](#platforms)): it needs systemd user scopes (cgroup v2), `/proc`,
`md5sum`, git; `rsync` and `ssh` for a remote.

```
regate build [REV]                      # binary for a commit, branch or the working tree
regate run [SET] [--bin REV]            # run a set, judge each case against history
regate diff A B [SET]                   # which cases' output or cost moved between A and B
regate report [--case NAME]             # crashes, slowdowns, memory growth over time
regate list [SET]                       # the cases of a set and which this machine skips
regate queue [--kind K] [-m 12G] -- CMD # run CMD when the machine has room for it
regate guard                            # the Claude Code hook
```

## Quick start

```
cargo install --git https://github.com/kurmetov/regate
cd ~/dev/app                     # any git repository
regate init                      # gate.conf, tests/gate.cases, .regate/limits.conf sized to this machine
$EDITOR tests/gate.cases         # name a few cases:
#   let small = data/small.csv
#   [quick]
#   small.json  convert $small -o $out
regate run                       # builds the working tree, runs [quick], records it
regate diff main tree            # what your uncommitted change moves, case by case
```

For a Cargo package `init` fills in `build` and `bin`; anything else gets the two keys to fill in.
It never overwrites a file that is there. `run` and `diff` exit 1 when a case fails, regresses or
differs, so CI can stop on them.

The case file format is in `src/cases.rs`: `let` names inputs (a `*` makes one case per match),
`[set]` lines start sets, attributes are `mem=`, `timeout=`, `repeat=`, `slower=`, `grow=`.
`$out` is the case's output; every `$input` it uses goes into its cache key by size and mtime.

## On a real machine

**The setup.** One workstation (59 GB, 32 threads). Up to about a dozen Claude Code sessions at a time
work on one Rust workspace. Before it merges, a change is checked on a regression set
of about 50 cases: the program run on real input files, a median of 7 s each, up to 4 minutes.

| | Before regate | With regate |
|---|---|---|
| builds per check | two, in release: the base and the change | one, of the changed tree, incremental; a tree is never built twice |
| one build | release with LTO on 2 threads: ~2 min after a small edit, 5.5 min cold | median 44 s, build profile without cross-crate LTO |
| wait to build | until no other `rustc` ran on the machine: 30+ min with nine sessions | median 0 s, 90th percentile 17 s |
| the set (~50 cases) | one after another, on both sides: ~15 min | in parallel, the base side from history: median 3.2 min |
| **one check** | **~20 min**, plus the wait to build | **~4 min** |
| out of memory | parallel cold builds ran the box out of memory | no OOM kills after the queue's first day |

Over 25 days with regate: 1,038 checks of the full set, roughly 250 hours of agent waiting saved
(not counting the waits to build); 33,039 jobs through the queue (9,832 builds, 22,965 heavy runs) from
558 agent sessions; a job peaked at a median 46 % of its memory cap (builds: 4.6 GB cap, 1.4 GB peak).

"Before" comes from the old check script and notes made at the time, not from a side-by-side run;
"with" from the queue's and the gate's own records.

Jobs peak at under half of what they ask for. That is why admission is by `MemAvailable` and
learned peaks, not by the sum of caps.

## gate.conf

In the project root, or in its data dir to keep it out of the tree. `build` and `bin` are needed to
build (`--bin PATH` runs a binary as it is); the rest is optional:

```
cases    = tests/gate.cases          # the case file
build    = make app                  # run in the build worktree <data>/wt; split on spaces, a script for more
bin      = out/app                   # what the build made, relative to that worktree
env      = CARGO_PROFILE_RELEASE_LTO=false   # the build's environment; part of the binary's name
tag      =                           # the binary's name after its tree; default a hash of build, bin and env
check    = scripts/check-outputs.sh  # given a case's output files; a non-zero exit fails the case
stale    = was kept of another input # text in a failed case's log that makes it `stale` (checked nothing)
volatile = elapsed_seconds           # JSON keys an output's size and hash leave out (a time taken)
measures = APP_MEASURES              # one more variable naming the measures file, besides REGATE_MEASURES
beside   = {stem}_*.{stem}           # files beside an input the program reads unasked: inputs too
inputs   = /input/base               # JSON pointers in a JSON input that name more inputs
```

A program can report accuracy against a reference it did not make: one `{"name": ..., "value": ...}`
line per measure (an error in %, lower is better) into the file `REGATE_MEASURES` names. A measure
that gets worse fails `run` and `diff` (`LESS ACCURATE`) even when the output bytes agree.

## Data

Everything lives in the project's `.regate/`, which keeps itself out of git. The project root is the main checkout of the
repository you are in, a worktree too, or `REGATE_ROOT`.

| path | what |
|---|---|
| `history.jsonl` | one line per build and per case run: time, status, wall and CPU without the queue wait, `wait_s` (the queue), `took_s` (start to end), `run` (one launch; `report` lists launches with their length and longest wait), peak RSS, output md5 per file, load; status `stale` is a run that checked nothing, a failure |
| `bin/<tree>.<tag>/<bin>` | one binary per git tree id; the working tree is snapshotted through a throwaway index |
| `wt/`, `target/` | the one worktree and target dir builds happen in, incrementally |
| `logs/<bin md5>/<case>.log` | the case's stdout and stderr; `logs/build-<tree>.log` for builds |
| `out/<bin md5>/` | outputs while hashing, kept only with `--keep`; `<case>.lock` gives two runs of one binary a case in turn |
| `remote.conf` | optional: `ssh = <opts> user@host`, `root = /path`; cases and `cargo test/clippy/check` run there (`--local`, `REGATE_BUILD_LOCAL=1` keep them here) |

Why it is fast:

* **Same sources, same binary.** A tree id that was built is never built again; a branch
  switch rebuilds only what changed, in the one build worktree.
* **Same binary and inputs, same result.** A case whose key (binary md5, arguments, input
  sizes and mtimes) already ran ok is taken from history; `--fresh` reruns it.
* `--only a,b` runs just the case being fixed.
* **An output that is well formed but empty fails**, given a `check`: a run on an empty input exits 0
  and hashes fine, only the project knows what empty looks like. Status `ok but check failed: <its first
  line>`, first run included; such a run is no baseline.

Why it is safe on a shared box: every case runs through the queue
(capped, no swap; a case with `mem=` at most `[light] job_memory`
goes to a lane of its own instead of waiting behind heavy ones), six at a time unless `-j N`; builds go
through the build queue with their environment fixed (gate.conf `env`), which also names the binary,
so runs of another build are no baseline. Queue time counts in neither the case nor the build.

What a run is judged on: the median over the last five other binaries' runs of the same
case. CPU (user+sys), not wall time, because other sessions' load moves wall time;
`SLOWER` past +15 % and 1 s, `MORE MEMORY` past +10 % and 100 MB (per case or set:
`slower=25%`, `grow=15%`), `output changed` when the md5 differs from the latest other
binary. `repeat=3` runs a case three times and keeps the least CPU and peak; outputs that
differ between the runs fail the case (`output differs between runs`). `--threads N` runs with
another thread count; output that depends on it fails. A crash is recorded with its signal
(`SIGSEGV`, `SIGABRT`, `SIGKILL: memory cap or OOM`) or `timeout`. `SOURCE_DATE_EPOCH` is set for
every case, so a program that stamps its output with the time can agree byte for byte.

A case with an input missing on this machine is reported `skipped`, not failed, and not written to history.

## The guard

`regate guard` reads a Claude Code `PreToolUse` hook's JSON on stdin and denies a Bash command that
breaks the machine's rules, with the reason and the command to run instead. Its rules come from
`guard.conf` (`REGATE_GUARD`, else `~/.config/regate/guard.conf`); without the file it denies nothing.
Each rule is on when its key is set:

```
root       = /home/me/dev/app                     # the checkout the sessions share
build      = regate queue --kind build --         # where bare cargo is sent
heavy      = regate queue --kind heavy -m <cap> --
heavy_programs = python* node                     # these, given a file over heavy_size, go to heavy
heavy_size = 100M
dirs       = /tmp ~/dev/*                         # mkdir only under root and these
push       = origin
merge_set  = quick                                # merging into main needs this set green
```

What each key does (every key, with defaults, is at the top of `src/guard.rs`):

* `root`: the checkout sessions share. In it (not in its worktrees) `git add -A`, `add` without a commit
  in the same command, `commit -a`, `--amend`, `stash`, `reset` without paths, `checkout .`, checkout
  of a branch, `clean`, and worktrees outside it are refused; `cargo fmt` is refused anywhere.
* `build`: bare `cargo build/test/clippy/check/run` is sent to this wrapper.
* `heavy`, `heavy_programs`, `heavy_size`: `systemd-run`, headless Chrome and the listed programs given
  a big file go through the heavy queue.
* `dirs`: `mkdir` only under `root` and these.
* `push`: the only remotes to push to.
* `merge_set`, `merge_into`, `merge_paths`: `git merge B` into main in the root needs a green
  `regate diff main tree <set>` of B's exact tree in history.
* Always: force push, `push --no-verify`, `pkill`/`killall`, and `pgrep -f` feeding a kill.

`claude-plugin/` packages the hook and a skill that tells agents how to use the queue and the gate.
Install it in Claude Code (the `regate` binary must be on PATH; without it the hook lets everything through):

```
/plugin install regate --marketplace kurmetov/regate
```

## The queue

`regate queue [--kind K] [-m 12G] [-t N] [--user U] -- CMD...` waits for a place under
the limits in `<data>/limits.conf` (or `--config FILE`; template: `limits.conf` here), then execs CMD
in a systemd scope capped at its memory (no swap) and CPU (`-t` threads = `CPUQuota`; a build
also gets `CARGO_BUILD_JOBS`). A project's own `build.sh` / `heavy.sh` can be thin wrappers over it
(`--kind build` / `--kind heavy`). Every project on the machine should name the same `--config`: the
queue is one per config file. `regate queue status` lists it;
`queue cancel ID` takes a waiting job of this session off it (`--force`: anyone's). A running job is not its to stop.

* A job that fits passes one the machine cannot hold yet; after 20 minutes of waiting the oldest
  such job holds those that would delay it, after 40 minutes everyone but the light lane, so it
  gets in. It holds nobody if nothing runs, or if MemAvailable + the running jobs' anonymous memory
  (not their caps) has not reached its cap + `min_available` in the last 10 minutes: its reason then
  says `will not get in`, with what it needs and the most seen, for its session to lower `-m`
  (`queue-room.txt`, beside the state, is that record). `queue status` shows each job's anonymous memory and peak beside its cap, and a hint when the
  last 5 runs of the same type and source all peaked under half of it.
* Memory of the machine is `MemAvailable`, not the sum of caps: a job starts if it leaves
  `min_available` with the job at its cap, the caps of jobs younger than 20 s counted as taken.
  `[total] max_caps` bounds the sum of running caps, since jobs can all peak at once;
  `[total] memory` only bounds one job's cap.
* Limits: `[total]` for the machine (`jobs`, `memory`, `threads`, `min_available`), one
  section per kind (the same keys, plus `job_memory` / `job_threads` as a job's defaults),
  `[user]` for every user and `[user.NAME]` for one (by prefix). A user is the Claude
  session (`CLAUDE_CODE_SESSION_ID`), else `--user` / `REGATE_QUEUE_USER`, else `$USER`.
  The file is read at every admission: edit it and the next job obeys.
* Also: `min_disk` in `[total]` (wait while the disk is that full); `data_ext` in `[total]`, the
  extensions of data files (`convert a.dat` and `convert b.dat` then learn their peaks apart, see Reservation); per kind `max_runtime`
  (`2h`; systemd ends the scope) and `cpu_weight` (share when busy, default 100). There is no
  `io_weight`: a user's systemd manager is usually not delegated the io controller, so it would be ignored.
* `[gate] keep_binaries = N`: after each build the gate keeps the N binaries used last, with
  their logs and run directories. `history.jsonl` is never pruned.
* Order: tickets in arrival order; a job held back only by its kind's or user's limit is
  passed over, one held back by the machine total holds everyone behind it (no starvation).
* A job holds an flock on its own file, inherited across exec and by its children: its place
  is free only once its last process is gone, however it died. A job no limit can hold is
  refused at once.
* Load test (`cargo test --test load`): 40 clients over two kinds, four users, an 8G job
  among 1-2G ones - no limit exceeded at any instant, all finish in 5.5 s (3 s of work), the
  big one starts after 0.5 s; a SIGKILLed job frees its place; scope caps are what the config
  says. Measured cost: 100 clients waiting at once, 6.2 s for 5 s of work, 0.4 s of CPU.

**Reservation.** `-m` or `job_memory` is the kill cap. After three finished runs of the same job (`job_key`) the job holds
1.5 x its largest peak of the last five + 512 MB against every limit, and `queue status`' `mem` column shows that. A job holding at most 4 GB passes any job that holds the queue.

## Platforms

**Linux** with systemd and cgroup v2: everything.

**Windows: WSL2**, where regate runs as it does on Linux once WSL starts systemd:

```
# in WSL: /etc/wsl.conf
[boot]
systemd=true
```

then `wsl --shutdown` from Windows and open WSL again. Check that the queue can cap a job:

```
stat -fc %T /sys/fs/cgroup                                  # cgroup2fs
systemd-run --user --scope -p MemoryMax=100M -p MemorySwapMax=0 true && echo ok
loginctl enable-linger "$USER"                              # keeps the user manager up between shells
```

If the first line prints `tmpfs`, WSL mounts cgroup v1 too: add `kernelCommandLine = cgroup_no_v1=all` under
`[wsl2]` in `%UserProfile%\.wslconfig` and restart WSL. Also:

* The queue sees the WSL VM's memory, not the host's: set `memory=` in `.wslconfig` first, then `regate init`.
* Keep repositories in the WSL filesystem (`~/...`), not under `/mnt/c`: builds are several times faster there.
* Run Claude Code inside WSL too, so the guard and the queue see the same machine. From a Windows shell,
  `wsl.exe --cd <dir> -- regate ...` reaches it.

**macOS, native Windows**: not supported yet. The guard is plain text matching and the gate needs little
(md5 in process, `ru_maxrss` in bytes on macOS); the queue is the work: macOS has no hard memory cap for a
process tree, native Windows would cap with Job Objects. Contributions welcome.

## License

MIT or Apache-2.0, at your option.
