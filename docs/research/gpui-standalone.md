# GPUI as a standalone app framework

Research for [#2](https://github.com/jnsdls/slopwatch/issues/2), checked on 2026-09-30 against zed-industries/zed `main`, crates.io, and GitHub.

## Answer

GPUI works for apps that aren't Zed, and plenty of people ship them. The catch is distribution. Zed stopped publishing GPUI to crates.io after 0.2.2 (October 2025), so a real app either pins a git rev of the Zed monorepo or uses Longbridge's `gpui-pre` snapshot crates through `gpui-kit`. Expect breaking changes on every bump.

Linux and Windows are close to free for rendering and windowing. Zed itself ships weekly on all three, and GPUI's platform crates are selected by `cfg` with no code changes on your side. They are not free for packaging, signing, CI, tray icons, or bug triage, and Windows and Linux carry more open GPUI issues than macOS.

A node-graph canvas is feasible. GPUI has a `canvas` element and a `PathBuilder` with bezier curves, and two community node-editor crates exist. Neither is mature, and GPUI has no element-level transform, so pan and zoom mean scaling coordinates yourself.

## Publishing and versioning

- The `gpui` crate on crates.io is at 0.2.2, published 2025-10-22. Versions 0.2.0 to 0.2.2 all came out in October 2025, and 0.1.0 from 2022 is yanked. ([crates.io/crates/gpui](https://crates.io/crates/gpui))
- On Zed `main`, `crates/gpui/Cargo.toml` still says `version = "0.2.2"`, but the workspace sets `publish = false`, and the platform code now lives in separate crates (`gpui_platform`, `gpui_macos`, `gpui_apple`, `gpui_linux`, `gpui_windows`, `gpui_wgpu`, `gpui_web`). None of them is on crates.io. ([Cargo.toml](https://github.com/zed-industries/zed/blob/main/Cargo.toml), [crates/gpui/Cargo.toml](https://github.com/zed-industries/zed/blob/main/crates/gpui/Cargo.toml))
- The GPUI README on `main` tells you to add `gpui_platform` and call `gpui_platform::application()`. That crate only exists in git, so the README's own instructions don't work from crates.io. ([crates/gpui/README.md](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md))
- A discussion asking for a new crates.io release after Zed 1.0 (2026-04-29) has had no answer from Zed staff. One reply points out that current `gpui` cannot be published by anyone because of git dependencies and `publish = false` on crates like `gpui_macros`. ([discussion #55271](https://github.com/zed-industries/zed/discussions/55271))
- There is also an open report that `gpui_apple/build.rs` reads `../gpui/src/scene.rs`, which breaks builds outside the monorepo layout. ([discussion #64406](https://github.com/zed-industries/zed/discussions/64406))
- Longbridge fills the gap. `gpui-pre` and `gpui-pre-platform` are crates.io snapshots of a recorded Zed commit (currently zed@1a28cff), published by huacnlee roughly weekly since 2026-09-03 (0.3.0 to 0.3.7). `gpui-kit` 0.7.0 pins `gpui-pre = 0.3.7` exactly. ([crates.io/crates/gpui-pre](https://crates.io/crates/gpui-pre), [gpui-kit installation docs](https://github.com/longbridge/gpui-kit/blob/main/website/docs/installation.md))
- A community fork, [gpui-ce/gpui-ce](https://github.com/gpui-ce/gpui-ce) (1.2k stars, created 2025-12-12), republishes the split crates as `gpui-ce`, `gpui_ce_platform`, and so on.
- Licensing: an earlier concern that `sum_tree` pulled GPL-3.0 crates (`ztracing`, `zlog`) into GPUI is resolved on `main`. Every Zed-internal crate reachable from `gpui` and `gpui_platform` is now Apache-2.0. ([discussion #50444](https://github.com/zed-industries/zed/discussions/50444); checked by walking each crate's `Cargo.toml`)

What real apps do, from their `Cargo.toml`:

| App | GPUI source |
| --- | --- |
| [AprilNEA/OpenLogi](https://github.com/AprilNEA/OpenLogi) (22.5k stars) | `gpui-pre =0.3.4` plus a forked gpui-kit |
| [zeronsh/zeron](https://github.com/zeronsh/zeron) (2.5k) | own fork `zeronsh/zui` at a pinned rev |
| [l0ng-ai/tty7](https://github.com/l0ng-ai/tty7) (1.2k) | Zed git rev, patched to own fork branch |
| [sonorahq/sonora](https://github.com/sonorahq/sonora) (1.7k) | own fork at a pinned rev |
| [egoist/waku](https://github.com/egoist/waku) (1.5k) | own Zed fork branch (`waku-webview`) |
| [penso/arbor](https://github.com/penso/arbor) (830) | crates.io `gpui = "0.2.2"` |

Most serious apps carry a fork. That tells me upstream moves too fast, or lacks something they need, for a plain pin to hold.

## API stability

- The README says: "still pre-1.0. There will often be breaking changes between versions. You'll also need to use the latest version of stable Rust." ([README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md))
- Churn is high. 168 commits touched `crates/gpui` between 2026-07-01 and 2026-09-30, and 201 PRs with "gpui" in the title merged in the same window. The platform split into `gpui_platform` and friends is one recent example of a breaking reshuffle.
- Zed develops GPUI for Zed. The gpui.rs site says contributions go through the Zed repo and "need to be ... kept in sync with it." ([gpui.rs](https://www.gpui.rs/))
- Docs are thin. The README's advice is to read the Zed source or ask on Discord. gpui-kit's docs at [gpui-kit.com](https://gpui-kit.com) are the most complete written guide.

## Platform maturity

| Platform | Renderer and text | Zed ships since | Open Zed issues (all / also `area:gpui`) |
| --- | --- | --- | --- |
| macOS | Metal, CoreText via `font-kit` | original platform | 46 / 5 |
| Linux | wgpu (Vulkan, GL fallback), cosmic-text; Wayland and X11 | [2024-07-10](https://zed.dev/blog/zed-on-linux) | 87 / 8 (plus Wayland 31/5, X11 18/9) |
| Windows | DirectX 11, DirectWrite | [2025-10-15](https://zed.dev/blog/zed-for-windows-is-here) | 99 / 8 |

Issue counts come from GitHub search on `zed-industries/zed` labels on 2026-09-30. Zed hit 1.0 on [2026-04-29](https://github.com/zed-industries/zed/releases/tag/v1.0.0) and ships weekly (v1.22.0 on 2026-09-30).

Notes that matter for slopwatch:

- Cross-platform code is `cfg`-selected inside `gpui_platform`. On macOS you must enable `font-kit` or text renders as blank. Linux needs `wayland` and/or `x11` features. Windows needs no features. ([README](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md))
- Linux is the fiddly one. It needs a working Vulkan driver in a graphical session, and several open PRs are still tuning wgpu backend selection (skip software Vulkan, skip GL when Vulkan works, choose a backend). ([#63346](https://github.com/zed-industries/zed/pull/63346), [#63313](https://github.com/zed-industries/zed/pull/63313), [#64227](https://github.com/zed-industries/zed/pull/64227))
- Windows builds need Visual Studio C++ build tools, the MSVC toolchain, and CMake. gpui-kit documents macOS 15+ and Windows 10+ as its platform floor. ([gpui-kit installation](https://github.com/longbridge/gpui-kit/blob/main/website/docs/installation.md))
- There is no tray or menu-bar icon support upstream (no `NSStatusItem` anywhere in the Zed repo). [#44047 "gpui: Add Tray support"](https://github.com/zed-industries/zed/pull/44047) closed unmerged on 2025-12-12; its author published a separate `gpui-tray` crate instead. If slopwatch wants a menu-bar presence, that is extra work on every platform.
- System notifications exist (`examples/system_notifications.rs`), and AccessKit accessibility is wired on all three platforms.

## Custom drawing and a node-graph editor

- `canvas(prepaint, paint)` gives direct access to the paint API inside a normal view. ([elements/canvas.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/elements/canvas.rs))
- `PathBuilder` supports `move_to`, `line_to`, `curve_to` (quadratic), `cubic_bezier_to`, `arc_to`, polygons, dash arrays, fill or stroke, and transform, translate, scale, and rotate on the path itself. Tessellation goes through lyon. ([path_builder.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/src/path_builder.rs), [examples/painting.rs](https://github.com/zed-industries/zed/blob/main/crates/gpui/examples/painting.rs))
- Upstream examples cover what a graph editor needs: `drag_drop.rs`, `painting.rs`, `paths_bench.rs`, `anchor.rs`, `input.rs`.
- The gap is zoom. `TransformationMatrix` shows up only for SVGs and sprites in the scene and shaders, not as a transform on arbitrary elements. Zooming a graph of div-based nodes means multiplying positions, sizes, and font sizes yourself, or drawing nodes entirely in `canvas`.
- Existing node-editor crates are young:
  - [tu6ge/ferrum-flow](https://github.com/tu6ge/ferrum-flow), "extensible node-based editor framework", 87 stars, last push 2026-06-07, on `gpui = "0.2.2"`.
  - [pacifio/gpui-flow](https://github.com/pacifio/gpui-flow), "React Flow for GPUI", 38 stars, a single day of pushes in March 2026, on a Zed git dep.
  - [gpui-whiteboard](https://github.com/packetThrower/zorite/tree/main/crates/gpui-whiteboard) inside Zorite is an infinite pan-and-zoom canvas with shapes and arrows. It is the closest active prior art.

My read: slopwatch's Pipeline graphs are small, tens of nodes. Absolutely positioned divs for nodes plus a `canvas` layer for bezier edges is a few hundred lines of our own code. Taking on either node-editor crate would mean a dependency with a different GPUI pin.

## Component libraries

- [longbridge/gpui-kit](https://github.com/longbridge/gpui-kit), formerly gpui-component, has 15.3k stars, active daily (pushed 2026-09-30), and is at 0.7.0 on crates.io. It ships 75+ components, including a dock layout, virtualized tables and lists, a Tree-sitter code editor, Markdown and HTML rendering, charts, theming, AccessKit, headless UI tests, and WebAssembly. Longbridge Pro, a commercial trading app, is built on it. ([README](https://github.com/longbridge/gpui-kit))
- Its crates are `gpui-kit` (single dependency, re-exports GPUI), `gpui-base` (unstyled behavior), and `gpui-component` (styled). The crate is Apache-2.0 (`LICENSE-APACHE` in the repo, `license` field on crates.io).
- Smaller options exist in [awesome-gpui](https://github.com/zed-industries/awesome-gpui), such as Bezel, but nothing near gpui-kit's scope.

## Apps outside Zed

[awesome-gpui](https://github.com/zed-industries/awesome-gpui) has about 130 table rows of apps and libraries. Ones relevant to slopwatch:

- [zeronsh/zeron](https://github.com/zeronsh/zeron): "A native control plane for Claude Code, Codex, Cursor, Devin and other coding agents." 2.5k stars since 2026-07-20. Close to slopwatch's space.
- [egoist/waku](https://github.com/egoist/waku), [maddada/Ghostex](https://github.com/maddada/Ghostex), [iAmCorey/Wake](https://github.com/iAmCorey/Wake), [penso/arbor](https://github.com/penso/arbor), [yicheng47/runner](https://github.com/yicheng47/runner), [reviu-dev/reviu](https://github.com/reviu-dev/reviu): coding-agent managers or review tools built on GPUI.
- [AprilNEA/OpenLogi](https://github.com/AprilNEA/OpenLogi), 22.5k stars, macOS/Windows/Linux, and [Longbridge Pro](https://longbridge.com/desktop) are the biggest non-Zed shipping apps.
- [l0ng-ai/tty7](https://github.com/l0ng-ai/tty7) uses the same daemon-plus-GPUI-client split slopwatch plans, with native builds on all three platforms.

## Open questions

- Which pin strategy slopwatch takes: `gpui-kit` with its `gpui-pre` snapshot, a raw Zed git rev, or a fork. gpui-kit is the lowest-effort path but ties our GPUI upgrades to Longbridge's cadence.
- Whether a menu-bar or tray presence matters for v1, since GPUI has none.
