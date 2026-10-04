//! Where the window's commands to the daemon go out, and which of them are
//! still waiting for an answer. Each command gets its [`RequestId`] here,
//! so an answer finds the action that sent it: a button shows a spinner
//! until then, a list shows it's loading, and a refusal shows at the
//! control. Every view holds a clone. No GPUI here, so it tests without a
//! window.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use slopwatch_protocol::{Command, RequestId, Response, ResponseBody};

#[derive(Clone)]
pub struct Outbox {
    link: Sender<(RequestId, Command)>,
    state: Rc<RefCell<State>>,
}

#[derive(Debug, Default)]
struct State {
    last_id: u64,
    /// The action each unanswered request was sent for, `None` for one no
    /// control waits on.
    waiting: HashMap<RequestId, Option<String>>,
    /// How each action's last answered request went, until it sends again.
    answers: HashMap<String, Result<(), String>>,
}

impl Outbox {
    /// Hands commands to the link on `link`.
    pub fn new(link: Sender<(RequestId, Command)>) -> Self {
        Self {
            link,
            state: Rc::default(),
        }
    }

    /// Sends `command` for no control in particular, such as a
    /// subscription. A refusal is the window's to show.
    pub fn send(&self, command: Command) {
        self.dispatch(None, command);
    }

    /// Sends `command` for the button `action`, unless the last one it sent
    /// is still out, so a double click sends one. True if it went.
    pub fn press(&self, action: &str, command: Command) -> bool {
        if self.waiting(action) {
            return false;
        }
        self.dispatch(Some(action), command);
        true
    }

    /// Asks for what the list `action` shows. Unlike a press, a second ask
    /// goes out while the first is waiting, so the listing after a change
    /// is never dropped.
    pub fn load(&self, action: &str, command: Command) {
        self.dispatch(Some(action), command);
    }

    fn dispatch(&self, action: Option<&str>, command: Command) {
        let mut state = self.state.borrow_mut();
        state.last_id += 1;
        let id = RequestId(state.last_id);
        if let Some(action) = action {
            state.answers.remove(action);
        }
        state.waiting.insert(id, action.map(str::to_owned));
        // The link thread only stops when the app quits.
        let _ = self.link.send((id, command));
    }

    /// Whether a request `action` sent is still waiting for its answer.
    pub fn waiting(&self, action: &str) -> bool {
        let state = self.state.borrow();
        state.waiting.values().flatten().any(|each| each == action)
    }

    /// Why the daemon refused the last request `action` sent.
    pub fn error(&self, action: &str) -> Option<String> {
        match self.state.borrow().answers.get(action) {
            Some(Err(message)) => Some(message.clone()),
            _ => None,
        }
    }

    /// Whether the daemon carried out the last request `action` sent.
    pub fn done(&self, action: &str) -> bool {
        matches!(self.state.borrow().answers.get(action), Some(Ok(())))
    }

    /// Drops how `action` last went, as a form does when it opens again.
    pub fn forget(&self, action: &str) {
        self.state.borrow_mut().answers.remove(action);
    }

    /// Takes the daemon's answer. True if an action waited on it, which
    /// then shows a refusal itself.
    pub fn answered(&self, response: &Response) -> bool {
        let mut state = self.state.borrow_mut();
        let Some(Some(action)) = state.waiting.remove(&response.id) else {
            return false;
        };
        let answer = match &response.result {
            ResponseBody::Ok(_) => Ok(()),
            ResponseBody::Error(error) => Err(error.message.clone()),
        };
        state.answers.insert(action, answer);
        true
    }

    /// The link went down or came back. What was waiting either never left
    /// or went to a connection that can no longer answer.
    pub fn reset(&self) {
        self.state.borrow_mut().waiting.clear();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc::{self, Receiver};

    use slopwatch_protocol::{ErrorBody, ErrorCode, Reply, RepoName};

    use super::*;

    fn outbox() -> (Outbox, Receiver<(RequestId, Command)>) {
        let (link, sent) = mpsc::channel();
        (Outbox::new(link), sent)
    }

    fn watch() -> Command {
        Command::Watch {
            repo: RepoName::new("o", "r"),
            number: 7,
        }
    }

    fn ok(id: RequestId) -> Response {
        Response {
            id,
            result: ResponseBody::Ok(Reply::Done),
        }
    }

    fn refused(id: RequestId, message: &str) -> Response {
        Response {
            id,
            result: ResponseBody::Error(ErrorBody {
                code: ErrorCode::GitHub,
                message: message.into(),
            }),
        }
    }

    #[test]
    fn a_press_waits_until_its_answer_arrives() {
        let (outbox, sent) = outbox();

        assert!(outbox.press("watch-7", watch()));
        let (id, command) = sent.try_recv().unwrap();
        assert_eq!(command, watch());
        assert!(outbox.waiting("watch-7"));
        assert!(!outbox.waiting("watch-8"));
        assert!(!outbox.done("watch-7"));

        assert!(outbox.answered(&ok(id)));
        assert!(!outbox.waiting("watch-7"));
        assert!(outbox.done("watch-7"));
        assert_eq!(outbox.error("watch-7"), None);
    }

    #[test]
    fn a_refusal_stays_with_its_action_until_it_sends_again() {
        let (outbox, sent) = outbox();
        outbox.press("watch-7", watch());
        let (id, _) = sent.try_recv().unwrap();

        assert!(outbox.answered(&refused(id, "GitHub said no")));
        assert!(!outbox.waiting("watch-7"));
        assert!(!outbox.done("watch-7"));
        assert_eq!(outbox.error("watch-7").as_deref(), Some("GitHub said no"));
        assert_eq!(outbox.error("watch-8"), None);

        outbox.press("watch-7", watch());
        assert_eq!(outbox.error("watch-7"), None, "a new try clears it");
    }

    #[test]
    fn a_double_click_sends_one_command() {
        let (outbox, sent) = outbox();

        assert!(outbox.press("watch-7", watch()));
        assert!(!outbox.press("watch-7", watch()));
        assert_eq!(sent.try_iter().count(), 1);
        assert!(outbox.press("watch-8", watch()), "another action goes");
    }

    #[test]
    fn a_list_waits_for_every_ask() {
        let (outbox, sent) = outbox();
        outbox.load("secrets", Command::ListSecrets);
        outbox.load("secrets", Command::ListSecrets);
        let ids: Vec<RequestId> = sent.try_iter().map(|(id, _)| id).collect();
        assert_eq!(ids.len(), 2, "a second ask isn't dropped");
        assert_ne!(ids[0], ids[1]);

        outbox.answered(&ok(ids[0]));
        assert!(outbox.waiting("secrets"));
        outbox.answered(&ok(ids[1]));
        assert!(!outbox.waiting("secrets"));
    }

    #[test]
    fn an_answer_no_action_waits_on_is_the_windows() {
        let (outbox, sent) = outbox();
        outbox.send(Command::Refresh);
        let (id, _) = sent.try_recv().unwrap();

        assert!(!outbox.answered(&refused(id, "nope")));
        assert!(!outbox.answered(&ok(RequestId(99))), "not one it sent");
    }

    #[test]
    fn a_reset_stops_the_waiting() {
        let (outbox, sent) = outbox();
        outbox.press("watch-7", watch());
        let (id, _) = sent.try_recv().unwrap();

        outbox.reset();
        assert!(!outbox.waiting("watch-7"));
        assert!(outbox.press("watch-7", watch()));
        assert!(!outbox.answered(&ok(id)), "the old connection's answer");
    }

    #[test]
    fn forgetting_clears_how_it_went() {
        let (outbox, sent) = outbox();
        outbox.press("paste", Command::Refresh);
        let (id, _) = sent.try_recv().unwrap();
        outbox.answered(&ok(id));

        outbox.forget("paste");
        assert!(!outbox.done("paste"));
    }
}
