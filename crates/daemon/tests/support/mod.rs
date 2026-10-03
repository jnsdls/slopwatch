//! Shared by the integration tests that run Step processes.

use std::path::Path;
use std::process::Command;

/// Kills, once dropped, every process group that has a process whose
/// command line names `marker`, a temporary path only one test's Step
/// scripts carry. A test daemon going away leaves its Steps running, as a
/// SIGKILLed daemon would, and they have to end with the test. A field of
/// a fixture that outlives each simulated crash keeps the groups alive
/// until the test is over, for the next daemon to find.
pub struct Reaper {
    marker: String,
}

impl Reaper {
    pub fn new(marker: &Path) -> Self {
        Self {
            marker: marker
                .to_str()
                .expect("temporary paths are UTF-8")
                .to_owned(),
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        let Ok(listing) = Command::new("ps")
            .args(["-A", "-ww", "-o", "pgid=,command="])
            .output()
        else {
            return;
        };
        // SAFETY: getpgrp takes nothing and touches no memory.
        let own = unsafe { libc::getpgrp() };
        for line in String::from_utf8_lossy(&listing.stdout).lines() {
            let Some((pgid, command)) = line.trim_start().split_once(' ') else {
                continue;
            };
            let Ok(pgid) = pgid.parse::<i32>() else {
                continue;
            };
            if pgid > 1 && pgid != own && command.contains(&self.marker) {
                // SAFETY: killpg takes plain integers. A group that's
                // already gone just returns ESRCH.
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            }
        }
    }
}
