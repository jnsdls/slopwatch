use std::time::Duration;

use futures_util::StreamExt;
use gpui_kit::*;
use slopwatch_client::link;
use slopwatch_client::main_view::MainView;

gpui_kit::actions!(slopwatch, [Quit]);

/// How often the GUI retries while the daemon is down.
const RETRY: Duration = Duration::from_secs(1);

fn main() {
    let (events, mut received) = futures_channel::mpsc::unbounded();
    let (commands, to_send) = std::sync::mpsc::channel();
    let socket = slopwatch_protocol::local_socket_path();
    std::thread::Builder::new()
        .name("daemon-link".to_owned())
        .spawn(move || {
            link::run(&socket, RETRY, &to_send, |event| {
                events.unbounded_send(event).is_ok()
            })
        })
        .expect("spawn the daemon link thread");

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_window_closed(|cx, _| cx.quit()).detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(960.), px(600.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("slopwatch".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (_, view) = gpui_kit::open_window(options, cx, |_, cx| {
            cx.new(|cx| MainView::new(commands, cx))
        })
        .expect("open the main window");
        cx.activate(true);

        let view = view.downgrade();
        cx.spawn(async move |cx| {
            while let Some(event) = received.next().await {
                let updated = view.update(cx, |view, cx| view.handle(event, cx));
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    });
}
