# GPUI comes from gpui-kit's `gpui-pre` snapshots

The client depends on `gpui-kit` from crates.io and gets GPUI through the `gpui-pre` version it pins exactly. Zed stopped publishing `gpui` after 0.2.2, and current GPUI lives only in the Zed monorepo with `publish = false`. `gpui-pre` is Longbridge's snapshot of a recorded Zed commit. We want gpui-kit's components anyway, and taking its pin gives us one copy of GPUI with no `[patch]` plumbing.

## Considered options

- A raw Zed git rev, with gpui-kit `[patch]`ed onto it, as tty7 does. It decouples us from Longbridge's cadence, but we would own the plumbing and gain nothing from it until we need an unreleased upstream change.
- Our own fork from day one, as zeron and sonora do. Nothing settled so far needs a GPUI patch. Notifications use GPUI's own `show_system_notification` in the client (ADR 0013), and the Dock badge and `SMAppService` are AppKit calls outside GPUI.

## Consequences

- Upgrades follow gpui-kit releases. Pins are exact and `Cargo.lock` is committed. A bump happens when a gpui-kit release has something we want, or before a slopwatch release if we're more than about a month behind. It lands as its own PR, together with the `rust-toolchain.toml` bump GPUI needs.
- Only the client crate may depend on GPUI or gpui-kit. Core, protocol and daemon crates stay GPUI-free, so a bump can't break the daemon. CI checks this with `cargo tree`.
- We take no GPUI crates outside gpui-kit, because each one carries its own GPUI pin. The Pipeline graph canvas is our own code.
- The escape hatch, if we need a patch upstream won't ship or the snapshots stop, is a fork of Zed at the commit `gpui-pre` records, redirected with `[patch.crates-io]`. Every patch goes upstream as a PR, and the fork goes away once they merge. If the snapshots stop, the same `[patch]` points at a plain Zed git rev.
