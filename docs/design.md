# crumb design

crumb manages per-worktree development backends ("leases"): one backend, one
database and whatever wiring it needs, per git worktree, so several branches
can run side by side. This document records how it works and why.

## Principles

- **crumb keeps no state of its own.** Every fact is read from the system that
  owns it: Mutagen sessions, Docker containers, Postgres databases, env files,
  tmux sessions. The lease name is the join key. There is no registry, slot
  file or state file to drift out of date.
- **Show before you change.** Destructive actions print the exact plan first.
  Databases are never removed automatically.
- **Every command is visible.** crumb shells out to `ssh`, `mutagen`,
  `docker` and `psql` the way lazygit shells out to `git`, and records each
  command, its duration and its exit status in a command log you can copy
  from.
- **The screen never waits.** All reads run on background threads; the UI
  redraws on a key press or new data.

## A lease is a worktree plus pieces

Each piece has a small, closed set of built-in drivers. Anything else is a
command you provide (see "Command contract").

| Piece    | Built-in drivers                    | Escape hatch |
| -------- | ----------------------------------- | ------------ |
| Host     | `local`, `ssh`                      |              |
| Code     | `none`, `mutagen`                   |              |
| Runtime  | `compose`, `process` (tmux)         |              |
| Port     | `direct`, `mutagen`, `ssh`          |              |
| Database | `none`, `postgres` copy             | command      |
| Wiring   | env files                           |              |
| Checks   | `deps`, `schema`                    | command      |

There is no plugin API. New built-in drivers ship in crumb itself.

### Example configurations

A VPS running Docker, code synced with Mutagen, databases created by a
script:

```toml
host = "ssh://indigo"

[code]
sync = "mutagen"
ignore = "mutagen.yml"

[runtime]
compose = "scripts/vps/compose.worktree.yml"
project = "wt_{lease}"
service = "backend"
port = 8080

[database]
name = "wt_{lease}"
server = "docker://supabase_db"
create = "bash scripts/branch-db.sh create {lease}"
drop = "bash scripts/branch-db.sh drop {lease}"

[checks]
deps.lockfile = "pnpm-lock.yaml"
schema.migrations = "db/migrations"

[[wire]]
file = "apps/web/.env.development.local"
set.API_URL = "http://127.0.0.1:{local_port}"
```

Docker on this machine:

```toml
[runtime]
compose = "compose.yml"
service = "web"
port = 3000

[database]
postgres = "postgres://localhost:5432"
from = "app_development"
name = "app_{lease}"

[[wire]]
file = ".env.local"
set.APP_URL = "http://localhost:{port}"
```

A dev server without Docker, databases on Neon:

```toml
[runtime]
start = "pnpm dev --port {port}"
ready = "http://localhost:{port}/health"

[database]
create = "neonctl branches create --name {lease} -o json"
drop = "neonctl branches delete {lease}"

[[wire]]
file = ".env.local"
set.DATABASE_URL = "{database.url}"
```

### Config files

- `crumb.toml` at the repo root, committed, describes the project.
- `~/.config/crumb/config.toml` holds machine-specific values such as the SSH
  host, so teammates share one project config.
- Precedence: flags, then `CRUMB_*` environment variables, then the user
  config, then the project config.
- Templates: `{lease}`, `{n}`, `{port}`, `{local_port}`, `{worktree}`,
  `{host}`, and any field a command prints (`{database.url}`).

## Identity

The lease name comes from the worktree directory (lowercased, anything outside
`[a-z0-9]` becomes `_`) or `--name`. Every piece is named from it or labeled
with it:

| Piece        | Owner              | Name                 | Metadata                                                          |
| ------------ | ------------------ | -------------------- | ----------------------------------------------------------------- |
| Code sync    | Mutagen daemon     | `wt-<lease>`         | label `crumb.lease=<lease>`                                       |
| Port forward | Mutagen daemon     | `wt-<lease>-api`     | label `crumb.lease=<lease>`                                       |
| Replica      | host filesystem    | `<root>/<lease>`     | the path is the name                                              |
| Containers   | Docker             | project from config  | labels `crumb.lease`, `crumb.port`, `crumb.db`, `crumb.worktree`  |
| Database     | Postgres           | name from config     | `COMMENT ON DATABASE` with worktree path, source and time         |
| Wiring       | the worktree       | configured env files | first-line marker `# crumb lease <lease>`                         |

Leases created before crumb are recognised through the compose project name
template, extra Mutagen label keys (`code.label_keys`) and the published
port.

## Ports

- One number `nn` from 01 to 99 per lease. The host port is `host_base + nn`
  and the local port is `local_base + nn`, so one tells you the other.
- Both ends bind to loopback. Some WebSocket clients treat only `localhost`
  and `127.0.0.1` as local and upgrade any other host to `wss:`, which breaks
  plain-HTTP dev backends reached over a LAN or tailnet IP.
- On `up`, `nn` is the lowest number not used by any container's `crumb.port`
  label (stopped containers included), any Mutagen forward, or any listener
  on either machine. Allocation and `compose up` run under a `flock` on the
  host so concurrent `up`s never collide.
- The remote port reaches this machine through a Mutagen forward session,
  which reconnects after sleep or a network change.
- A lease may get a different port after `down` and `up`; crumb rewrites the
  wired env files each time.

## Code sync (Mutagen)

- One one-way replica per lease, from the worktree to the host, with the
  project's ignore list.
- Read with `mutagen sync list --template '{{json .}}'`; no text parsing.
- `mutagen sync flush` returns before the first scan lands, so readiness is
  "`successfulCycles` went up and status is `watching`".
- Stop pauses the session; down terminates it and deletes the replica.

## Runtime (Docker Compose)

- Compose runs on the host (over SSH for remote hosts). Running compose from
  this machine against a remote daemon resolves bind mounts and env files to
  local paths.
- The compose file is streamed to the host on each `up`.
- Recommended restart policy: `on-failure:5`. It caps crash loops, and leases
  stay stopped after a host reboot instead of all starting at once.
- Dependency drift: the SHA-256 of the lockfile inside the image (computed
  once per image id) against the worktree's lockfile.

## The probe

One bash script, compiled into crumb and sent to the host on stdin
(`ssh host bash -s`), so nothing is installed on the host. It prints
sections: container facts as one JSON object per line (never environment
variables, which hold secrets), memory from cgroup files, listeners, and
databases from one `psql` query. One SSH round trip over a shared connection
(`ControlMaster`, `ControlPersist`, `ClearAllForwardings`) costs about 55 ms;
a fresh connection costs about a second and is paid once.

## Databases

- Created and dropped through the configured command, or by the `postgres`
  driver (dump and restore; `CREATE DATABASE … TEMPLATE` fails while anything
  is connected to the source).
- crumb adds a database comment with its origin, so a database without a
  lease still explains itself.
- One backend per database, checked before every start: two backends on one
  database take each other's queued jobs.
- Schema freshness: newest migration in the worktree against newest applied
  revision in the database.
- Dropping takes an explicit key and typing the lease name.

## Lifecycle

States are derived from each snapshot:

- **starting**, **healthy**, **crashed** (five failed starts, or restarting),
  **exited** (non-zero), **stopped**
- **orphaned**: the worktree path no longer exists on this machine
- **database only**: a database matching the template with nothing else

| Key | Verb    | Does                                                                 | Keeps          |
| --- | ------- | -------------------------------------------------------------------- | -------------- |
| u   | up      | lock, allocate, database, sync, compose, forward, wire, wait healthy | —              |
| r   | restart | wait for sync, restart the service, wait healthy                     | everything     |
| s   | stop    | stop containers, pause sync and forward                              | everything     |
| d   | down    | remove containers, forward, sync, replica, wiring                    | the database   |
| D   | drop    | down, then drop the database                                         | nothing        |
| R   | reap    | orphans: stop now; down after seven days stopped                     | the database   |
| m   | migrate | apply migrations to the lease database, restart                      | everything     |
| t   | tunnel  | recreate the forward and rewrite wiring                              | everything     |

`up` is idempotent: each step ensures its piece exists, so rerunning after a
failure continues rather than duplicating. The seven-day grace clock is
Docker's own `State.FinishedAt`. A worktree manager's removal hook (for
example an Orca archive hook) can call `crumb down`; reap catches the rest.

## Command contract

A command driver receives `CRUMB_LEASE`, `CRUMB_WORKTREE`, `CRUMB_PORT`,
`CRUMB_LOCAL_PORT` and `CRUMB_HOST`. It writes progress to stderr, exits 0 on
success, and may print one JSON object on stdout; its fields become template
variables and status (`{"url": "...", "state": "ok", "detail": "..."}`).
Commands run on actions and when a lease's details are opened, never in the
refresh loop. Orca environment recipes and Conductor scripts use the same
shape.

## Interface

- One list, no boxes, rules, panes or tabs. A healthy lease is one line.
- Groups in the order you act on them: needs you, running, orphaned,
  databases only. With one group there is no heading.
- One status phrase per lease instead of per-layer columns; a layer speaks up
  only when it is behind.
- The selected row expands in place with the reason and the key that fixes
  it. Logs and the command log are full-screen views left with `esc`.
- Reap and drop show their plan in place of the list; one key applies it.
- Colors come from the terminal's 16-color palette: red for broken, yellow
  for behind, blue for the selection and keys, dim for everything secondary.
- The footer lists only the keys that apply to the selected row.
- CLI output uses the same words, plain text when piped, `--json` for agents.
  `crumb up` prints one right-aligned verb per step, like cargo.

## Setup

- Install: `mise use -g github:peaske7/crumb`, `brew install
  peaske7/tap/crumb`, the shell installer from the GitHub release, or `cargo
  binstall crumb-cli` (the crates.io name `crumb` is taken). One release
  built by `dist`, macOS and Linux, arm64 and x64.
- `crumb init` detects the compose file, Mutagen config, SSH hosts,
  migrations and env files, asks at most three questions with the detected
  answer preselected, and writes a commented `crumb.toml`.
- `crumb doctor [--json]` checks each configured piece and prints the command
  that fixes each failure.
- `crumb agents install|uninstall|status [--client claude-code|codex|all]
  [--project]` installs the agent skill, which is compiled into the binary so
  `status` can report drift. The repo ships `skills/crumb/SKILL.md` for
  `npx skills add peaske7/crumb`.

## Prior art

lazygit and k9s (interface, command log), terraform plan (plans before
changes), Kubernetes controllers and Ansible (idempotent reconcile, agentless
SSH), Tilt (per-layer status), preview environments (lifetime follows the
branch), `git worktree prune` (grace period), DDEV (the cost of treating a
missing directory as deleted), cargo (step output), Conductor and Orca
(worktree lifecycle scripts).
