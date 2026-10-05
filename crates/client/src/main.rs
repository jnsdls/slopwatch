use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;

use futures_util::StreamExt;
use gpui_kit::*;
use slopwatch_client::agent::Agent;
use slopwatch_client::link::{self, Pace};
use slopwatch_client::main_view::{self, MainView};
use slopwatch_client::notifications;
use slopwatch_client::outbox::Outbox;
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
            #[cfg(debug_assertions)]
            if slopwatch_client::demo::enabled() {
                slopwatch_client::demo::run(&to_send, |event| events.unbounded_send(event).is_ok());
                return;
            }
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

    // The daemon launches the GUI this way to post notifications while no
    // GUI runs (ADR 0013): no window and no focus until the developer asks.
    let background = std::env::args().any(|arg| arg == "--background");
    let main_window: Rc<Cell<Option<AnyWindowHandle>>> = Rc::default();
    // The component icons, such as the spinner on a button whose command is
    // out.
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);
    // Clicking the Dock icon with the window hidden shows it.
    let reopened = Rc::clone(&main_window);
    app.on_reopen(move |cx| show(reopened.get(), cx));

    app.run(move |cx| {
        gpui_kit::init(cx);
        slopwatch_client::theme::install(cx);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
        cx.on_window_closed(|cx, _| cx.quit()).detach();

        let (min_width, min_height) = main_view::MIN_WINDOW;
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1200.), px(760.)), cx)),
            window_min_size: Some(size(px(min_width), px(min_height))),
            titlebar: Some(TitlebarOptions {
                title: Some(Flavor::CURRENT.app_name().into()),
                ..Default::default()
            }),
            show: !background,
            focus: !background,
            ..Default::default()
        };
        let (window, view) = gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| MainView::new(Outbox::new(commands), agent, reregister, window, cx))
        })
        .expect("open the main window");
        main_window.set(Some(window));
        if !background {
            cx.activate(true);
        }

        // A click on a banner shows the window on the banner's PR.
        let clicked_view = view.downgrade();
        cx.on_system_notification_response(move |response, cx| {
            show(Some(window), cx);
            if let Some(pr) = notifications::clicked(&response.tag) {
                let _ = clicked_view.update(cx, |view, cx| {
                    view.reveal(pr);
                    cx.notify();
                });
            }
        });

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

/// Brings the app forward with its main window shown.
fn show(window: Option<AnyWindowHandle>, cx: &mut App) {
    cx.activate(true);
    if let Some(window) = window {
        let _ = window.update(cx, |_, window, _| window.activate_window());
    }
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
