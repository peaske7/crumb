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
crumb  indigo · 9.4 GB free · 7 leases                                 21:44

needs you
▌ lym_1119            lym-1119-2          :8107   exited 1 · 22m ago
    deps    image is older than your lockfile
    wired   frontends dial a non-loopback address, so live sockets will fail

orphaned · 1.6 GB                                                    R reap
  lym_1136            worktree gone       :8106   up 2d              378M
  lym_1127            worktree gone       :8101   crash loop · 25,943 119M

databases only
  wt_pr919_e2e   wt_saml_sso
```

## Status

Early. The read-only view (`crumb`, `crumb ls`) is being built first; see
[`docs/tasks`](docs/tasks) for the plan and [`docs/design.md`](docs/design.md)
for how it works.

## Install

From source until the first release:

```sh
cargo install --git https://github.com/peaske7/crumb
```

Planned channels: `mise use -g github:peaske7/crumb`,
`brew install peaske7/tap/crumb`, a shell installer, and
`cargo binstall crumb-cli`.

## Use

```sh
crumb            # the TUI
crumb ls         # the same list as text
crumb ls --json  # for scripts and agents
```

crumb reads `crumb.toml` from the repository root. A minimal config for a
remote Docker host synced with Mutagen:

```toml
host = "ssh://my-dev-box"

[code]
sync = "mutagen"

[runtime]
project = "wt_{lease}"
service = "backend"
port = 8080

[database]
name = "wt_{lease}"
server = "docker://postgres"
```

More configurations, including local Docker and dev servers without Docker,
are in [`docs/design.md`](docs/design.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.
