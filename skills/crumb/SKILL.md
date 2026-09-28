---
name: crumb
description: Run, check and clean up per-worktree development backends ("leases") with the crumb CLI. Use when a git worktree needs its own backend, database or port; when the user asks to start, restart, stop or tear down a worktree's backend; when a backend on a lease is failing, crash-looping or stale; when frontends in a worktree can't reach their backend; or when orphaned leases need reaping. Triggers: "crumb", "lease", "worktree backend", "spin up a backend for this branch", "why doesn't my change show up", "restart pending", "deps behind".
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
| `deps_behind` | the lockfile differs from the image's; the next start may fail on a missing module | rebuild the image, then `crumb restart` |
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
crumb logs -f     # follows the backend's log
crumb reap --dry-run   # what reap would do to orphaned leases
```

`crumb up` is safe to rerun: each step continues where the last run stopped.
It prints the URL on this machine when the backend is healthy. Tell the user
to restart dev servers that read the wired env files.

## Rules

- Never run `crumb drop` (it deletes the lease database) or `crumb reap`
  without the user's explicit yes. Show `crumb reap --dry-run` first.
- One backend per database: never point two leases at one database. `up`
  refuses; don't work around it.
- Don't report a lease as up until `crumb up` finished or `crumb ls` says
  `healthy`.
- `crumb -v <command>` prints every command crumb ran, with timings, when
  something needs debugging.
