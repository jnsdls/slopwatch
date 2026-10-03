use std::time::Duration;

use futures_util::StreamExt;
use gpui_kit::*;
use slopwatch_client::link;
use slopwatch_client::status_view::StatusView;

gpui_kit::actions!(slopwatch, [Quit]);

/// How often the GUI retries while the daemon is down.
const RETRY: Duration = Duration::from_secs(1);

fn main() {
    let (statuses, mut received) = futures_channel::mpsc::unbounded();
    let socket = slopwatch_protocol::local_socket_path();
    std::thread::Builder::new()
        .name("daemon-link".to_owned())
        .spawn(move || {
            link::watch(&socket, RETRY, |status| {
                statuses.unbounded_send(status).is_ok()
            })
        })
        .expect("spawn the daemon link thread");

    gpui_kit::application().run(move |cx| {
        gpui_kit::init(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_window_closed(|cx, _| cx.quit()).detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(480.), px(280.)), cx)),
            titlebar: Some(TitlebarOptions {
                title: Some("slopwatch".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (_, view) = gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| StatusView::new()))
            .expect("open the main window");
        cx.activate(true);

        let view = view.downgrade();
        cx.spawn(async move |cx| {
            while let Some(status) = received.next().await {
                let updated = view.update(cx, |view, cx| view.set_status(status, cx));
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
    });
}
