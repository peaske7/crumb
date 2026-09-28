# Working on crumb

Read `docs/design.md` before changing behaviour, and the task packet in
`docs/tasks/` for what is next.

## Rules

- crumb keeps no state of its own. Never add a registry, cache file or state
  file; read facts from the system that owns them.
- Every external command goes through the runner so it lands in the command
  log. No `std::process::Command` calls elsewhere.
- The probe must never print container environment variables.
- Nothing destructive runs without a plan the user has seen. Databases are
  never dropped automatically.
- The UI thread never blocks on I/O.
- Plain `std` threads and channels; no async runtime.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests exercise behaviour through public functions (facts in, rows out; probe
output in, facts out). The TUI is verified by running it.
