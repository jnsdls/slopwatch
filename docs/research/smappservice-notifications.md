# Notifications and executables in an SMAppService agent

Research for [#47](https://github.com/jnsdls/slopwatch/issues/47). The question is whether the daemon, running as the launchd agent registered with `SMAppService.agent`, can post macOS notifications through `UNUserNotificationCenter`, and what that means for how many executables the `.app` holds.

Checked on 2026-10-02 against the macOS 26.2 SDK headers, Apple's documentation, Apple developer forum threads answered by Apple staff, open-source code, and a small local probe on macOS 15.7.3.

## Short answer

Don't plan on the launchd agent posting notifications through `UNUserNotificationCenter`. Apple DTS says it won't work, and nothing Apple publishes says it will. The daemon can stay a bare, GPUI-free Rust binary. Notifications come from a process that Launch Services starts as an app, and the main app is the simplest candidate. Two executables in one bundle, one bundle ID for notifications, one permission prompt, and a click lands in the GUI.

## Findings

### 1. A bare executable can't use `UNUserNotificationCenter` at all

`+[UNUserNotificationCenter currentNotificationCenter]` asserts with `bundleProxyForCurrentProcess is nil` when the process has no main bundle. A developer hit this with an unbundled launch agent in [forum thread 679326](https://developer.apple.com/forums/thread/679326). GPUI's own macOS notification code checks `NSBundle.mainBundle.bundleIdentifier` first for the same reason ([zed PR #61189](https://github.com/zed-industries/zed/pull/61189), `crates/gpui_macos/src/system_notifications.rs`). The local probe reproduced the abort with a binary outside any bundle.

### 2. A second executable in `Contents/MacOS` takes the app's bundle ID

Foundation picks the main bundle from the executable's path, so an executable in `Slopwatch.app/Contents/MacOS/` gets `Slopwatch.app` as its main bundle even when it isn't `CFBundleExecutable`. The probe confirmed this when run from a shell and when launchd spawned it (`launchctl submit`). The extra executable reported the app's `CFBundleIdentifier` and got a notification settings object back without aborting. That holds even with its own code signing identifier (`codesign -i com.example...daemon`).

An executable inside a nested helper `.app` (for example `Contents/Library/LoginItems/Helper.app`) instead reports the helper's own bundle ID.

So "one binary in two modes" and "a second executable in `Contents/MacOS`" look the same to `UNUserNotificationCenter`. Both resolve to the main app's bundle ID. Only a nested helper `.app` gets a separate identity.

### 3. Apple DTS says the User Notifications framework won't work from a launchd agent

In [forum thread 804854](https://developer.apple.com/forums/thread/804854) (October 2025) a developer with "a launch agent (within the app bundle)" got authorization granted, but every `addNotificationRequest` failed with `UNErrorCodeNotificationsNotAllowed`. Posting the same request from the main app worked. Quinn "The Eskimo!" of DTS gave the accepted answer:

> While User Notifications framework won't work for you, you do have other options, albeit more old school ones. A launchd agent can present a notification using the CFUserNotification type.

A second Apple engineer in the same thread said notifications need a user-level TCC context, and suggested moving the notification code into a user-level process that the agent talks to.

`CFUserNotification` is not a replacement. Its header describes it as a dialog "for the use of processes that do not otherwise have user interfaces", with a header, message, text fields and up to three buttons (`CoreFoundation/CFUserNotification.h`). It's a modal alert, not a Notification Center banner, and it doesn't show up in Notification Center history.

The probe adds one data point. Neither launchd-spawned process, the `Contents/MacOS` executable or the nested helper, had a Launch Services app record (`lsappinfo find pid=...` returned nothing). That's consistent with the DTS answer. It doesn't prove the failure mechanism.

### 4. A nested helper app can probably post, with its own permission

In [forum thread 732291](https://developer.apple.com/forums/thread/732291) a developer moved an agent into its own app bundle with its own signing identifier and found that it "can't send notifications with UNUserNotificationCenter because I only requested authorization in the app". The helper needed its own authorization, which implies it could post once it had one. That agent was started by the app, not by `SMAppService`. Quinn's reply says that with `SMAppService`, "TCC should be able to track responsibility from it to your app", but he said that about TCC in general, not about notifications.

Mac Mouse Fix ships this layout. Its `SMAppService` plist sets `BundleProgram` to `Contents/Library/LoginItems/Mac Mouse Fix Helper.app/Contents/MacOS/Mac Mouse Fix Helper` with `ProcessType` `Interactive` ([sm_launchd.plist](https://github.com/noah-nuebling/mac-mouse-fix/blob/master/Shared/HelperServices/sm_launchd.plist)). Its helper doesn't use `UNUserNotificationCenter`, so it shows the layout works for UI but says nothing about notifications. Summond embeds its agent as `Contents/MacOS/SummondAgent.app` with its own bundle ID and `LSUIElement`, so that TCC prompts name the agent ([ARCHITECTURE.md](https://github.com/gjermundgaraba/summond/blob/main/ARCHITECTURE.md)).

I found no open-source app where an `SMAppService` agent posts `UNUserNotificationCenter` notifications itself. Thread 804854 doesn't say whether its agent was a bare executable or a nested `.app`, so it doesn't settle the helper case either way.

### 5. The working pattern is a Launch Services app that owns notifications

Every working example I found puts `UNUserNotificationCenter` in a process launched as an app:

- In thread 679326 the developer's fix was a small menu bar app. The agent launches it, the two talk over XPC, and the app posts. "macOS automatically launches notification handler app when user clicks notification."
- agi-cli's menu bar helper posts from a one-shot launched with `open MenubarHelper.app --args --notify ...`. "Responses land on the persistent instance (same bundle id)", which handles `didReceive` and opens the target ([phnx-labs/agi-cli#3545](https://github.com/phnx-labs/agi-cli/pull/3545)).
- Ally kept notifications in the signed main app, used `SMAppService.mainApp`, and deferred a background helper "until cross-process identity behavior can be validated" ([Ally-Personal-AI/ally#121](https://github.com/Ally-Personal-AI/ally/pull/121)).

### 6. The click goes to the app that owns the bundle ID

Apple's documentation for `userNotificationCenter(_:didReceive:withCompletionHandler:)` says the response arrives on the notification center's delegate, and can indicate that the user "launched your app". The `UNUserNotificationCenter.h` comment says the delegate "must be set before the application returns from application:didFinishLaunchingWithOptions:". So the app must install its delegate during launch to catch the response for a click that launched it. GPUI's implementation installs the delegate and forwards `request().identifier()` as the response tag. If the notification's identifier encodes the PR, the GUI gets it on click and can open that PR.

If a nested helper posts instead, the click goes to the helper's bundle ID. The helper then has to open the GUI itself, for example with `NSWorkspace` and a `slopwatch://` URL, which would need `CFBundleURLTypes` in the main app's `Info.plist`.

Unverified: what happens when a launchd-spawned executable shares the main app's bundle ID and has a delegate set, and the user clicks. Apple doesn't document it, and finding 3 suggests the post fails before this matters.

### 7. Signing and Info.plist

- `SMAppService.h` says apps using the API "must be code signed". Agents (unlike daemons) don't need notarization.
- An executable in `Contents/MacOS` needs no `Info.plist` of its own. It reads the app's. Its code signing identifier defaults to its file name unless set with `codesign -i`. The probe showed that the signing identifier doesn't change `Bundle.main`.
- A nested helper `.app` needs its own `Info.plist` with `CFBundleIdentifier`, `CFBundleExecutable`, and `LSUIElement` to stay out of the Dock. It's signed as nested code before the outer app, and asks for notification permission under its own name.
- `BundleProgram` "is only supported for plists that are installed using SMAppService" (`man launchd.plist`). Apple's [migration guide](https://developer.apple.com/documentation/servicemanagement/updating-helper-executables-from-earlier-versions-of-macos) uses `Contents/Resources/mydaemon` as its example location, so the helper doesn't have to live in `Contents/MacOS`.
- No entitlement is needed for local notifications. Push notifications would need `aps-environment`, which slopwatch doesn't use.

## Options

1. **The daemon posts from `Contents/MacOS`.** Ruled out by finding 3 unless a signed build proves DTS wrong.
2. **The daemon is a nested helper `.app` and posts itself.** Plausible, but unverified (finding 4). It adds a second notification identity with its own permission prompt. The daemon must link AppKit or `UserNotifications`, so it needs an Objective-C runtime path, and a click has to bounce to the GUI.
3. **The daemon asks the main app to post.** The daemon stays a bare Rust binary. When it needs a notification and no GUI client is connected, it asks Launch Services to open the main app in the background (`open -g -j`, or `NSWorkspace` with `activates = false`) with the notification's details in a URL or argument. The GUI posts with GPUI's `show_system_notification` and receives the click under `com.jnsdls.slopwatch`. When a GUI is connected, the daemon sends the notification over the socket instead. Costs: a GUI process starts to post a notification while the user had quit it, and the GUI needs a windowless start mode. It matches what Apple staff recommended in thread 804854 and the working examples in finding 5.

I'd go with option 3. It keeps the daemon GPUI-free and keeps one bundle ID and one permission prompt, and the click lands in the process that opens the PR. Option 2 is the fallback if starting the GUI in the background turns out to be unacceptable.

## What's still unverified

- Whether a `UNUserNotificationCenter` post from the `SMAppService` agent fails on a properly signed build. The DTS answer says yes, but the thread doesn't show the layout. A signed N=1 test settles it in minutes. It needs a person to click "Allow", which is why the probe here stopped at reading settings.
- Whether a nested helper `.app` registered through `SMAppService.agent` can post after its own authorization (option 2).
- Whether starting the GUI in the background through Launch Services avoids showing a window or stealing focus with GPUI's current startup code (option 3).
- Click routing when two processes share a bundle ID (finding 6).

## Probe

A Swift program that prints `Bundle.main` and calls `UNUserNotificationCenter.current().getNotificationSettings`, copied to three places and ad-hoc signed:

| Location | `Bundle.main` | Result |
| --- | --- | --- |
| `/tmp/sw47/probe` (no bundle) | `/tmp/sw47`, no identifier | `NSException`, abort |
| `Probe.app/Contents/MacOS/daemon`, signed `-i ...daemon` | `Probe.app`, `com.example.sw47probe` | settings returned, status 0 |
| `Probe.app/Contents/Library/Helper.app/.../Helper` | `Helper.app`, `com.example.sw47probe.helper` | settings returned, status 0 |

Launchd-spawned runs (`launchctl submit`) gave the same bundle results. Neither launchd-spawned process had a Launch Services record. The probe never requested authorization or posted, so it doesn't answer finding 3.
