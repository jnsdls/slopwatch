use std::sync::Arc;

use futures_util::StreamExt;
use gpui_kit::*;
use slopwatch_client::agent::Agent;
use slopwatch_client::link::{self, Pace};
use slopwatch_client::main_view::MainView;
use slopwatch_protocol::{Flavor, socket_path};

gpui_kit::actions!(slopwatch, [Quit]);

fn main() {
    let agent = this_bundles_agent();
    let (events, mut received) = futures_channel::mpsc::unbounded();
    let (commands, to_send) = std::sync::mpsc::channel();
    let (reregister, reregister_requests) = std::sync::mpsc::channel();
    let socket = socket_path(&Flavor::CURRENT.data_dir());
    let link_agent = agent.clone();
    std::thread::Builder::new()
        .name("daemon-link".to_owned())
        .spawn(move || {
            let agent = link_agent.as_deref();
            let controls = link::Controls {
                commands: &to_send,
                reregister: &reregister_requests,
            };
            link::run(&socket, Pace::GUI, agent, controls, |event| {
                events.unbounded_send(event).is_ok()
            });
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
                title: Some(Flavor::CURRENT.app_name().into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (_, view) = gpui_kit::open_window(options, cx, |_, cx| {
            cx.new(|cx| MainView::new(commands, agent, reregister, cx))
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

#[cfg(target_os = "macos")]
fn this_bundles_agent() -> Option<Arc<dyn Agent>> {
    let agent = slopwatch_client::agent::LaunchAgent::of_this_bundle()?;
    Some(Arc::new(agent))
}

#[cfg(not(target_os = "macos"))]
fn this_bundles_agent() -> Option<Arc<dyn Agent>> {
    None
}
