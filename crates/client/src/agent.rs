//! The daemon's launchd agent, which the GUI registers with
//! `SMAppService.agent` from the plist in its bundle (ADR 0009).

/// What `SMAppService` reports about the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentStatus {
    NotRegistered,
    Enabled,
    /// The developer turned it off in Login Items, or hasn't approved it.
    RequiresApproval,
    /// macOS doesn't know the agent, such as before its first register.
    NotFound,
}

/// The agent as the GUI drives it. The real one is [`LaunchAgent`]; tests
/// fake it.
pub trait Agent: Send + Sync {
    fn status(&self) -> AgentStatus;

    /// Unregisters, waits for macOS to finish, then registers, so launchd
    /// starts the binary the bundle holds now. `SMAppService.h` asks for
    /// this after an executable changes.
    fn reregister(&self) -> Result<(), String>;

    /// Opens System Settings at Login Items, where the developer approves
    /// the agent.
    fn open_login_items(&self);
}

#[cfg(target_os = "macos")]
pub use launch_agent::LaunchAgent;

#[cfg(target_os = "macos")]
mod launch_agent {
    use std::sync::mpsc;
    use std::time::Duration;

    use block2::RcBlock;
    use objc2_foundation::{NSBundle, NSError, NSString};
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    use slopwatch_protocol::Flavor;

    use super::{Agent, AgentStatus};

    /// How long to wait for `unregister`'s completion handler.
    const UNREGISTER_TIMEOUT: Duration = Duration::from_secs(10);

    /// The agent whose plist sits in this app's
    /// `Contents/Library/LaunchAgents`.
    pub struct LaunchAgent {
        plist_name: String,
    }

    impl LaunchAgent {
        /// The agent of the running bundle. `None` when the GUI runs outside
        /// a bundle of its own flavor, such as from `cargo run`, where there
        /// is no plist to register.
        pub fn of_this_bundle() -> Option<Self> {
            let bundle_id = NSBundle::mainBundle().bundleIdentifier()?;
            if bundle_id.to_string() != Flavor::CURRENT.bundle_id() {
                return None;
            }
            Some(Self {
                plist_name: format!("{}.plist", Flavor::CURRENT.agent_label()),
            })
        }

        fn service(&self) -> objc2::rc::Retained<SMAppService> {
            // SAFETY: the plist name is a valid NSString, and the call only
            // builds a handle; nothing touches launchd yet.
            unsafe {
                SMAppService::agentServiceWithPlistName(&NSString::from_str(&self.plist_name))
            }
        }
    }

    impl Agent for LaunchAgent {
        fn status(&self) -> AgentStatus {
            // SAFETY: status takes no arguments.
            match unsafe { self.service().status() } {
                SMAppServiceStatus::Enabled => AgentStatus::Enabled,
                SMAppServiceStatus::RequiresApproval => AgentStatus::RequiresApproval,
                SMAppServiceStatus::NotRegistered => AgentStatus::NotRegistered,
                _ => AgentStatus::NotFound,
            }
        }

        fn reregister(&self) -> Result<(), String> {
            let (done, finished) = mpsc::channel();
            let handler = RcBlock::new(move |error: *mut NSError| {
                // SAFETY: the framework passes null or a valid NSError.
                let error = unsafe { error.as_ref() }.map(describe);
                let _ = done.send(error);
            });
            // SAFETY: the block stays alive for the call, and the framework
            // copies it before calling it later.
            unsafe { self.service().unregisterWithCompletionHandler(&handler) };
            // Unregistering an agent macOS doesn't have fails, and registering
            // is what matters either way.
            match finished.recv_timeout(UNREGISTER_TIMEOUT) {
                Ok(Some(error)) => eprintln!("slopwatch: unregister failed: {error}"),
                Ok(None) => {}
                Err(_) => eprintln!("slopwatch: unregister didn't finish, registering anyway"),
            }
            // SAFETY: register takes no arguments and reports through NSError.
            unsafe { self.service().registerAndReturnError() }.map_err(|error| describe(&error))
        }

        fn open_login_items(&self) {
            // SAFETY: takes no arguments.
            unsafe { SMAppService::openSystemSettingsLoginItems() };
        }
    }

    fn describe(error: &NSError) -> String {
        format!(
            "{} ({} {})",
            error.localizedDescription(),
            error.domain(),
            error.code()
        )
    }
}
