# crumb

A fast, minimal TUI and CLI for per-worktree development backends.

When every git worktree gets its own backend, database and port, the pieces
end up spread across your machine and a dev server: sync sessions, containers,
databases, env files, tunnels. Delete a worktree and its backend keeps
running. Change a lockfile and the backend dies on the next restart without
telling anyone.

crumb shows every worktree's backend in one list, says what is behind and
why, and starts, restarts and cleans them up. It keeps no state of its own:
everything is read from the systems that own it.

```
crumb  indigo · 8.5 GB free · 8 leases                                    23:38

running
▌ lym_1119              lym-1119-2      :8107    wired to 100.64.0.1      473M
    port      nothing forwards :8107 to this machine; t creates it
    wiring    apps/hub/.env.development.local points at http://100.64.0.1:8107;
              WebSocket clients upgrade non-loopback hosts to wss:, so live
              connections fail. t rewires through a forward
  crumb                 crumb           :18108   schema behind            390M

orphaned · 1.6G   R reap
  lym_1127              worktree gone   :8101    crash loop · 26,607      513M
  lym_1136              worktree gone   :8106    up 2d                    385M

databases only
  wt_pr919_e2e   wt_saml_sso
```

## Status

Usable: crumb runs the first user's (Lymo's) worktree backends. The read
path, `up`, the lifecycle verbs, reap, `init`, `doctor` and the agent skill
work against a remote Docker host synced with Mutagen. See
[`docs/tasks`](docs/tasks) for what is left and
[`docs/design.md`](docs/design.md) for how it works.

## Install

Until the first release, from source:

```sh
cargo install --git https://github.com/peaske7/crumb
crumb agents install   # teach Claude Code and Codex to use crumb
```

From the first release on: `mise use -g github:peaske7/crumb`, the shell
installer attached to each GitHub release, `brew install
peaske7/tap/crumb` and `cargo binstall crumb-cli`. macOS and Linux, arm64 and
x64.

To update, use the same method you installed with:

```sh
mise upgrade github:peaske7/crumb
brew upgrade peaske7/tap/crumb
cargo binstall crumb-cli                                   # replaces an older release
curl -LsSf https://github.com/peaske7/crumb/releases/latest/download/crumb-cli-installer.sh | sh
cargo install --git https://github.com/peaske7/crumb --locked   # from source
```

`crumb --version` prints the commit a build came from (`0.1.0 (1a2b3c4)`),
so you can tell a stale install from a current one.

## Use

```sh
crumb              # the TUI
crumb ls           # the same list as text; --json for scripts and agents
crumb up           # start this worktree's backend; safe to rerun
crumb restart      # wait for the sync, restart, wait until healthy
crumb stop         # stop it and pause its sync; keeps everything
crumb down         # remove it; keeps the database
crumb drop         # down, then drop the database (asks you to type the name)
crumb migrate      # apply this worktree's migrations to its database, restart
crumb tunnel       # recreate the port forward, rewrite the wired env files
crumb rebuild      # rebuild the image (checks.deps.rebuild), start on it
crumb logs -f      # follow the backend's log
crumb reap         # stop orphans now; bring down those stopped seven days
crumb init         # write a crumb.toml from what the repository contains
crumb doctor       # check every piece; print the fix for each failure
crumb -v <cmd>     # also print every command crumb ran, with timings
```

In the TUI: `j`/`k` move, `⏎` details, `u` up, `r` restart, `s` stop,
`t` tunnel, `m` migrate, `b` rebuild, `d` down, `D` drop, `R` reap, `l` logs, `c` the
command log (`y` copies a command), `q` quit. The footer shows only the keys
that apply to the selected lease. Down, drop and reap show their plan first.

## Configure

crumb reads `crumb.toml` from the worktree (for branches that predate it,
from the main checkout, then from the default branch; `--config` or
`CRUMB_CONFIG` names another), then
`~/.config/crumb/config.toml` over it for what differs by machine, then
`CRUMB_HOST` and `--host`. A remote Docker host synced with Mutagen:

```toml
host = "ssh://my-dev-box"

[code]
sync = "mutagen"
root = "~/wt"                 # replicas live at ~/wt/<lease> on the host
ignore = "mutagen.yml"        # or a list: ["node_modules/", "dist/"]

[runtime]
compose = "compose.worktree.yml"
project = "wt_{lease}"
service = "backend"           # the service crumb waits for
port = 8080                   # its port inside the container
env_file = "~/app/.env"       # --env-file, on the host
up_args = ["--renew-anon-volumes"]
subnet = "172.16.{n}.0/24"    # the default network, from the lease's number
memory_mb = 700               # refuse `up` when it would leave less than
keep_free_mb = 3072           # keep_free_mb available on the host

[ports]
host_base = 8100              # 127.0.0.1:81nn on the host
local_base = 18100            # the same lease on 127.0.0.1:181nn here

[database]
name = "wt_{lease}"
server = "docker://postgres"  # or a postgres:// URL
from = "app_development"      # copied for each lease; or your own command:
# create = "./scripts/db create {lease}"
# drop = "./scripts/db drop {lease}"
migrate = "DATABASE_URL=postgres://localhost/{database} npm run migrate"
migrate_on_up = true          # migrate a new lease before its first start

[checks]
deps = { lockfile = "pnpm-lock.yaml", image_path = "/app/pnpm-lock.yaml", rebuild = "make image" }
schema = { migrations = "db/migrations", query = "select max(version) from schema_migrations" }

[[wire]]
file = "web/.env.development.local"   # crumb owns this file
seed = "web/.env.local"               # copied from the main checkout if missing
set.API_URL = "http://127.0.0.1:{local_port}"
```

The compose file publishes the service as `127.0.0.1:${CRUMB_PORT}:8080`
and can use `${CRUMB_DATABASE}` and `${CRUMB_LEASE}`; crumb adds its labels
itself. Commands get `CRUMB_LEASE`, `CRUMB_PORT`, `CRUMB_LOCAL_PORT`,
`CRUMB_DATABASE` and `CRUMB_WORKTREE`, and may print one JSON object whose
fields become template values (`{database.url}`). Local Docker and dev
servers without Docker are in [`docs/design.md`](docs/design.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
