# slopwatch

## Layout

A Cargo workspace with four crates under `crates/`:

- `core`: the framework-free domain model.
- `protocol`: the frames clients and the daemon exchange (ADR 0010).
- `daemon`: the `slopwatchd` binary.
- `client`: the `slopwatch` GUI, the only crate allowed to depend on GPUI (ADR 0005). `scripts/check-gpui-isolation.sh` enforces that in CI.

## Installing

```sh
scripts/install.sh          # release build, /Applications/slopwatch.app
scripts/install.sh --dev    # dev build, ~/Applications/slopwatch-dev.app
```

The script builds the GUI and the daemon, bundles them with the daemon's launchd agent plist, signs the bundle ad-hoc, quits the running GUI, swaps the bundle and relaunches it. On first launch the GUI registers the daemon with `SMAppService`, and from then on launchd keeps it running whether or not the GUI is open. Re-running the script is the update. The new GUI sees the old daemon's build id and restarts it, then unregisters and registers the agent. On an ad-hoc build that takes 20 to 25 s (ADR 0009).

A dev build and a release build have their own bundle ID, launchd label and data dir (`~/Library/Application Support/slopwatch-dev` and `slopwatch`), so they run side by side. The daemon logs to `daemon.log` in its data dir.

## Running by hand

```sh
cargo run -p slopwatch-daemon   # a dev daemon on ~/Library/Application Support/slopwatch-dev/daemon.sock
cargo run -p slopwatch-client   # opens a window showing whether it reached the daemon
```

A plain cargo build is a dev build, so it shares the installed dev daemon's data dir, and a second daemon there exits because the first holds its lock. Set `SLOPWATCH_DATA_DIR` for both to run them on a directory of their own. Run outside the bundle, the GUI has no agent to register, so it only reports a daemon from another build.
