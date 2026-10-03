# The GUI posts every notification

Amended by [#69](https://github.com/jnsdls/slopwatch/issues/69): the notification id also names its PR, and the background GUI keeps a hidden window (see "What the build settled").

The daemon never posts a macOS notification. Apple DTS says the User Notifications framework doesn't work from a launchd agent, and any executable in `Contents/MacOS` borrows the app's bundle ID anyway, so a second binary gains nothing. The daemon decides what deserves a notification and keeps a record of it (id, title, body, target PR) until a client acks that it posted it. A connected GUI gets the record over the socket. With no GUI connected, the daemon runs `open -g -j -b <bundle id> --args --background`, and the GUI starts without a window or focus, connects, pulls the unacked records and posts them. The daemon stays a bare binary with no GPUI and no AppKit, and notifications, the permission prompt and the click all belong to `com.jnsdls.slopwatch`.

## Considered options

- A nested helper `.app` as the launchd agent, with its own bundle ID and `LSUIElement`, posting directly. It doesn't need the GUI, but it shows up as a second app in Notification settings with its own prompt, needs a `slopwatch://` URL scheme to send a click to the GUI, and nobody has shown that an agent in a helper bundle can post either. It stays the fallback if a signed build shows the background launch doesn't work.
- Pass the notification to a background launch in a URL or arguments. That's a second delivery path next to the socket, plus a URL scheme.
- Leave the GUI closed and hold notifications until the developer opens it. Notifications exist because the UI may be closed (Human Step and attention model), so this defeats them.

## Consequences

- The notification identifier is the record id: the Inbox entry id, or `shippable:<run>`. A repost after a crash or reconnect replaces the banner. When an Inbox entry closes, the GUI removes its delivered banner so Notification Center never lists a stale ask.
- A background-launched GUI stays running with a Dock icon and no window until the developer quits it. The Dock icon carries the badge.
- Quitting the GUI closes the UI, not slopwatch. The next notification launches it again. To silence slopwatch, the developer turns notifications off in System Settings or unwatches the PR.
- A click activates the GUI, opens the main window if none is open, and selects the record's PR with its Inbox entry. If the entry closed in the meantime, the PR pane opens on its latest Run.
- The GUI asks for notification permission at the onboarding tour's first repo, not at launch. If the developer denies it, the Inbox and badge still work, the Inbox pane says notifications are off and links to System Settings, and the GUI acks records without posting them.
- Dev builds launch `com.jnsdls.slopwatch.dev`, which matches their separate agent (ADR 0009).
- Three things need a signed build to confirm: the agent really can't post, the background launch shows no window and takes no focus with GPUI's startup code, and a click reaches the GUI while the daemon shares its bundle ID. If the background launch fails, the helper `.app` above replaces it.

## What the build settled

[#69](https://github.com/jnsdls/slopwatch/issues/69) built this on gpui-kit 0.7.0 (`gpui-pre` 0.3.7) and checked it on macOS 15.7 with an ad-hoc dev bundle.

- The pinned `gpui-pre` has system notifications (zed #61189): `show_system_notification` posts through `UNUserNotificationCenter` with the tag as the request identifier, so a repost replaces the banner. `dismiss_system_notification` removes a delivered or pending banner by tag, and `on_system_notification_response` hands back the clicked tag. The client uses them. GPUI can't read the permission or ask for it before the first post, so the client calls `UNUserNotificationCenter` through `objc2-user-notifications` for those two.
- The id names the PR as well as the record: `inbox:<entry>@<owner>/<name>#<n>`, `shippable:<run>@...` or `merged:<run>@...`. A banner left in Notification Center by an earlier GUI process still opens its PR, since the click hands back only the identifier. A cause opens the first PR it held when it opened.
- A Run that ends merged gets a banner too. It was shippable before its Merge Step ran.
- The daemon keeps notifications in SQLite until a client acks one. An acked banner about an Inbox entry stays recorded until the entry closes. Then the daemon sends a Retract, and the GUI removes the banner and acks it. An entry that closes before any GUI posted it drops its notification, so it never posts.
- The daemon launches the GUI when something is pending and no client is subscribed to `notifications`. It waits 10 s after its own start, since `open` on a GUI that's already running reopens it and shows its window, and a GUI connected before a restart reconnects within that time. A launch that doesn't produce a subscriber within 60 s is retried.
- A background launch opens the main window hidden (`show` and `focus` false) instead of opening none. The view, its state and its link events then live in one place. A click or a click on the Dock icon shows the window, and the window draws its current state when it appears. #56 saw a visible background window stay on "Connecting" until activated. A hidden window never shows that stale frame.
- Checked live: started by hand with one notification pending and no GUI, the daemon ran `open -g -j -b com.jnsdls.slopwatch.dev --args --background`. The GUI came up with no window on screen, the frontmost app didn't change, and the GUI acked the notification within a few seconds. `open -b` again, as a Dock click does, showed the window with its connected state drawn.
- Launch Services won't resolve `open -b` to a bundle under `/tmp`, even after `lsregister`. Real installs live in `/Applications` and `~/Applications`, so that only matters for manual tests.
- Notification Center records the app by path and designated requirement. An ad-hoc requirement is the cdhash, so permission likely resets with each rebuild, the way Keychain access does in ADR 0009. This is unconfirmed.
- Not checked, because each needs a person at the Mac: answering the permission prompt, seeing a banner, clicking it with the daemon sharing the bundle ID, and whether permission survives a rebuild. Nobody tested whether the agent can post either. The daemon never tries, so the answer changes nothing here.
