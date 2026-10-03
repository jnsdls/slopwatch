//! What the Library editor knows: the Library Steps the daemon last listed,
//! which one is open, and when the open text should be replaced. No GPUI
//! here, so it tests without a window.

use slopwatch_protocol::LibraryStep;

/// The text a new Library Step starts with. It passes the daemon's check,
/// so creating a Step never fails on its content.
pub const NEW_STEP_TEXT: &str = "# What this Step checks, and why.\nuses: jev\nwith: {}\n";

#[derive(Debug, Default)]
pub struct LibraryEditor {
    /// `None` until the daemon first lists the Library.
    steps: Option<Vec<LibraryStep>>,
    open: Option<String>,
    /// The text of the open Step as the daemon last listed it, which the
    /// editor compares against to tell unsaved edits.
    saved: Option<String>,
    /// A Step just created, to open once the daemon lists it.
    pending: Option<String>,
}

impl LibraryEditor {
    pub fn loaded(&self) -> bool {
        self.steps.is_some()
    }

    pub fn steps(&self) -> &[LibraryStep] {
        self.steps.as_deref().unwrap_or_default()
    }

    pub fn open_step(&self) -> Option<&LibraryStep> {
        let open = self.open.as_deref()?;
        self.steps().iter().find(|step| step.name == open)
    }

    /// Whether `text` in the editor differs from the open Step's saved text.
    pub fn unsaved(&self, text: &str) -> bool {
        self.saved.as_deref().is_some_and(|saved| saved != text)
    }

    /// Opens `name` and returns the text the editor should show.
    pub fn open(&mut self, name: &str) -> Option<String> {
        let text = self
            .steps()
            .iter()
            .find(|step| step.name == name)?
            .text
            .clone();
        self.open = Some(name.to_owned());
        self.saved = Some(text.clone());
        Some(text)
    }

    /// Opens `name` once the daemon lists it, such as a Step just created.
    pub fn open_when_listed(&mut self, name: &str) {
        self.pending = Some(name.to_owned());
    }

    /// Takes a fresh listing from the daemon, given what the editor holds
    /// now. Returns the text the editor should show instead, if it should
    /// change: the first Step on the first listing, a Step just created,
    /// the next Step when the open one is gone, or the open Step's new text
    /// when the editor has no unsaved edits. Unsaved edits are never
    /// replaced, so a refused save keeps them.
    pub fn listed(&mut self, steps: Vec<LibraryStep>, editor: &str) -> Option<String> {
        let unsaved = self.unsaved(editor);
        self.steps = Some(steps);
        if let Some(pending) = self.pending.take()
            && let Some(text) = self.open(&pending)
        {
            return Some(text);
        }
        match self.open_step().map(|step| step.text.clone()) {
            Some(text) if unsaved => {
                // Keep the edits, but compare them against the new text.
                self.saved = Some(text);
                None
            }
            Some(text) if self.saved.as_deref() == Some(text.as_str()) => None,
            Some(text) => {
                self.saved = Some(text.clone());
                Some(text)
            }
            None => {
                let first = self.steps().first()?.name.clone();
                self.open(&first)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, text: &str) -> LibraryStep {
        LibraryStep {
            name: name.into(),
            text: text.into(),
            problem: None,
        }
    }

    fn presets() -> Vec<LibraryStep> {
        vec![
            step("claude-fix", "uses: fix\n"),
            step("claude-review", "uses: claude\n"),
        ]
    }

    #[test]
    fn the_first_listing_opens_the_first_step() {
        let mut library = LibraryEditor::default();

        assert_eq!(library.listed(presets(), ""), Some("uses: fix\n".into()));
        assert_eq!(library.open_step().unwrap().name, "claude-fix");
        assert!(!library.unsaved("uses: fix\n"));
        assert!(library.unsaved("uses: fix\nwith: {}\n"));
    }

    #[test]
    fn a_saved_edit_comes_back_without_touching_the_editor() {
        let mut library = LibraryEditor::default();
        library.listed(presets(), "");
        let edited = "uses: fix\nwith: { agent: codex }\n";

        let after_save = vec![
            step("claude-fix", edited),
            step("claude-review", "uses: claude\n"),
        ];

        assert_eq!(library.listed(after_save, edited), None);
        assert!(!library.unsaved(edited));
    }

    #[test]
    fn a_refused_save_keeps_the_unsaved_edits() {
        let mut library = LibraryEditor::default();
        library.listed(presets(), "");

        assert_eq!(library.listed(presets(), "uses: lib/x\n"), None);
        assert!(library.unsaved("uses: lib/x\n"));
    }

    #[test]
    fn a_change_from_elsewhere_reaches_an_editor_without_edits() {
        let mut library = LibraryEditor::default();
        library.listed(presets(), "");

        let changed = vec![step("claude-fix", "uses: fix\n# changed\n")];

        assert_eq!(
            library.listed(changed, "uses: fix\n"),
            Some("uses: fix\n# changed\n".into())
        );
    }

    #[test]
    fn a_created_step_opens_once_listed() {
        let mut library = LibraryEditor::default();
        library.listed(presets(), "");
        library.open_when_listed("docs-check");
        let mut steps = presets();
        steps.push(step("docs-check", NEW_STEP_TEXT));

        assert_eq!(
            library.listed(steps, "uses: fix\n"),
            Some(NEW_STEP_TEXT.into())
        );
        assert_eq!(library.open_step().unwrap().name, "docs-check");
    }

    #[test]
    fn deleting_the_open_step_opens_the_first_one_left() {
        let mut library = LibraryEditor::default();
        library.listed(presets(), "");
        library.open("claude-review");

        let left = vec![step("claude-fix", "uses: fix\n")];

        assert_eq!(
            library.listed(left, "uses: claude\n"),
            Some("uses: fix\n".into())
        );
        assert_eq!(library.open_step().unwrap().name, "claude-fix");
    }

    #[test]
    fn an_empty_library_opens_nothing() {
        let mut library = LibraryEditor::default();

        assert_eq!(library.listed(Vec::new(), ""), None);
        assert!(library.loaded());
        assert!(library.open_step().is_none());
    }

    #[test]
    fn the_new_step_text_passes_the_daemons_check() {
        assert_eq!(slopwatch_core::check_library_step(NEW_STEP_TEXT), Ok(()));
    }
}
