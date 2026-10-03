# slopwatch

## Layout

A Cargo workspace with four crates under `crates/`:

- `core`: the framework-free domain model.
- `protocol`: the frames clients and the daemon exchange (ADR 0010).
- `daemon`: the `slopwatchd` binary.
- `client`: the `slopwatch` GUI, the only crate allowed to depend on GPUI (ADR 0005). `scripts/check-gpui-isolation.sh` enforces that in CI.

## Running by hand

```sh
cargo run -p slopwatch-daemon   # listens on ~/Library/Application Support/slopwatch/daemon.sock
cargo run -p slopwatch-client   # opens a window showing whether it reached the daemon
```

Set `SLOPWATCH_SOCKET` to a path for both to run a second daemon next to the installed one.
