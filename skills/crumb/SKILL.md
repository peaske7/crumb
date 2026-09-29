---
name: crumb
description: Run, check and clean up per-worktree development backends ("leases") with the crumb CLI. Use when a git worktree needs its own backend, database or port; when the user asks to start, restart, stop or tear down a worktree's backend; when a backend on a lease is failing, crash-looping or stale; when frontends in a worktree can't reach their backend; when orphaned leases need reaping; or when crumb itself fails, misreports a lease, or had to be worked around. Triggers: "crumb", "lease", "worktree backend", "spin up a backend for this branch", "why doesn't my change show up", "restart pending", "deps behind".
---

# crumb

crumb gives each git worktree its own backend: a database, a code sync, a
container or process, a port forwarded to this machine, and env files that
point the worktree's apps at it. The project's `crumb.toml` says how. crumb
keeps no state of its own; every fact comes from Docker, Mutagen, Postgres,
tmux and the worktree.

## Look first

```sh
crumb ls --json   # every lease: group, state, reasons, ports, worktree
crumb ls          # the same, for people
```

`group` is where to act: `needs_you` (broken), `running`, `orphaned` (its
worktree is gone), `database_only`. `reasons` says why. Soft reasons mean
"behind" and keep the lease running:

| reason | means | fix |
| --- | --- | --- |
| `restart_pending` | files the backend mounts changed after it started | `crumb restart` |
| `deps_behind` | the lockfile differs from the image's; the next start may fail on a missing module | `crumb rebuild` (needs `checks.deps.rebuild`; otherwise rebuild the image yourself, then `crumb up`) |
| `schema_behind` | the worktree has migrations the lease database lacks | `crumb migrate` |
| `no_forward` | nothing forwards the port to this machine | `crumb tunnel` |
| `wiring_stale`, `wiring_remote` | the worktree's env files point at the wrong port, or at a non-loopback host (WebSockets then fail) | `crumb tunnel` |

Hard reasons (`crash_loop`, `exited`, `unhealthy`, `sync_*`) need a look:
`crumb logs <lease> --tail 100` shows why.

## Do

Run these inside the worktree; the lease name defaults to the worktree's.

```sh
crumb up          # database, sync, port, runtime, forward, env files; waits until healthy
crumb restart     # waits for the sync to land, restarts, waits until healthy
crumb stop        # stops it and pauses the sync; keeps everything
crumb down        # removes containers, forward, sync, replica, env files; keeps the database
crumb migrate     # applies the worktree's migrations to the lease database, restarts
crumb tunnel      # recreates the port forward and rewrites the env files
crumb rebuild     # rebuilds the image from the worktree's lockfile, starts on it
crumb logs -f     # follows the backend's log
crumb reap --dry-run   # what reap would do to orphaned leases
```

`crumb up` is safe to rerun: each step continues where the last run stopped.
It prints the URL on this machine when the backend is healthy, and a
`Schema  N migrations behind` line when the new database lags the worktree
(`crumb migrate` fixes it). Tell the user to restart dev servers that read
the wired env files.

## Rules

- Never run `crumb drop` (it deletes the lease database) or `crumb reap`
  without the user's explicit yes. Show `crumb reap --dry-run` first.
- One backend per database: never point two leases at one database. `up`
  refuses; don't work around it.
- Don't report a lease as up until `crumb up` finished or `crumb ls` says
  `healthy`.
- `crumb -v <command>` prints every command crumb ran, with timings, when
  something needs debugging.

## When crumb surprises you

crumb gets better from what goes wrong in real use. Report it when a crumb
command fails with a message that doesn't say what to do, when `crumb ls`
disagrees with what is actually running, or when you had to reach for
`docker`, `mutagen` or `ssh` to do something crumb should have done.

1. Finish the user's task first; the report comes after.
2. Collect the command and its full output, `crumb --version`,
   `crumb -v <command>` if it happens again, `crumb ls --json` and
   `crumb doctor --json`. In the TUI, `c` shows the command log and `y`
   copies it.
3. Look for an existing issue:
   `gh issue list --repo peaske7/crumb --state all --search "<words>"`.
   If one matches, draft a comment on it instead of a new issue.
4. Draft the issue: what happened, what you expected, the smallest repro,
   and what crumb could do instead. Show the draft to the user and file it
   with `gh issue create --repo peaske7/crumb --label observed` only after
   they say yes.
5. The repository is public. Leave out host names, IP addresses, user names,
   home paths, database and project names, env values and anything from the
   user's data; say "a remote host" or "the shared dev database" instead.
