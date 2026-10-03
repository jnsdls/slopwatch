# The GUI posts every notification

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
