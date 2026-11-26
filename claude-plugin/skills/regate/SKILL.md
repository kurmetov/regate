---
name: regate
description: Builds, tests and heavy runs on a machine many sessions share go through the regate queue; checking one gate case while working and the full regate diff before merging into main; what a guard refusal means. Use before any cargo build/test/clippy/check, before a long run (a conversion, a benchmark, a big input), before a merge into main, when the guard refused a command or a job waits in the queue.
---

# regate: the queue and the gate

The machine is shared by many sessions; parallel cold builds have run it out of memory. So everything that
takes memory or CPU goes through the queue, and the guard hook refuses a way around it.

## Build and run

```
regate queue --kind build [-m 8G] [-t N] -- cargo build -p <crate> ...   # cargo build/test/clippy/check
regate queue --kind heavy [-m 12G] -- <command>                          # long runs: conversions, benchmarks
```

A project may wrap these in its own scripts; when the guard names a wrapper, use that one.

- Build one crate (`-p`), not the whole workspace; no `cargo fmt --all`.
- Longer than a minute: run it in the background.

## The queue

```
regate queue status        # what runs, what waits and why; each job's memory against its cap
regate queue cancel <id>   # take your own waiting job off
```

`will not get in` in the reason: the job does not fit even on an empty machine, lower `-m`. Never touch
another session's running job. Limits are `<data>/limits.conf`, read at every admission.

## The gate

- A project with no `gate.conf` yet: `regate init` writes it, the case file and `limits.conf`; it overwrites nothing.
- While working, only your own case: `regate run <SET> --only <case>` and the crate's unit tests.
- Before merging into main, once, the full `regate diff main tree <set>` in the background: it is long.
  The guard refuses `git merge` into main without a green run of that set on the branch's exact tree.
- `LESS ACCURATE` / `MEASURE GONE` is a failure: a reference is held worse. Time and memory only warn.
- Do not start your own batch of runs (a loop over every input) unless that is the task.

## A guard refusal

The hook's message names the rule and the right command: run that, do not rephrase the command to get past it.
Common ones: bare `cargo build` → the queue or the wrapper it names; `git add -A` in the shared root → add
your files by name; `mkdir` outside the project → keep files in the project or the scratchpad, or ask the user.
