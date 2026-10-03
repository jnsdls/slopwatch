//! Byte streams the daemon serves clients over. Every transport carries the
//! same WebSocket frames, so a test over [`in_process`] exercises the same
//! code as a GUI on [`unix`].

pub mod in_process;
pub mod unix;
