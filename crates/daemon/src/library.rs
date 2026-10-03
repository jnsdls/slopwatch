//! The developer's Library: one file per Library Step at
//! `~/.config/slopwatch/steps/<name>.yml`, on the daemon's machine.
//!
//! Pipelines reference Library Steps live, so [`Library::step`] reads the
//! file each time it's asked. A Pipeline's resolver calls it, so a saved
//! edit reaches the next load of every Pipeline that uses the Step.

use std::collections::BTreeSet;
use std::fmt;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use slopwatch_core::{PRESETS, check_library_step, is_library_step_name};
use slopwatch_protocol::LibraryStep;

/// Overrides the config dir that holds the Library, for tests and for
/// running a second daemon by hand next to the installed one.
pub const CONFIG_DIR_ENV: &str = "SLOPWATCH_CONFIG_DIR";

/// The file in the Library dir that lists the presets it has been given,
/// one name per line, so a preset the developer deleted stays deleted.
const SEEDED: &str = ".presets";
const EXTENSION: &str = "yml";

pub struct Library {
    dir: PathBuf,
}

#[derive(Debug)]
pub enum LibraryError {
    /// The name can't be one file in the Library.
    BadName(String),
    /// The text wouldn't load. The message names the Step.
    Invalid(String),
    NotFound(String),
    Io(io::Error),
}

impl fmt::Display for LibraryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LibraryError::BadName(name) => write!(
                f,
                "`{name}` can't name a Library Step; use lowercase letters, digits, `-` and `_`"
            ),
            LibraryError::Invalid(message) => f.write_str(message),
            LibraryError::NotFound(name) => write!(f, "There's no Library Step `{name}`"),
            LibraryError::Io(error) => write!(f, "Can't read or write the Library: {error}"),
        }
    }
}

impl From<io::Error> for LibraryError {
    fn from(error: io::Error) -> Self {
        LibraryError::Io(error)
    }
}

impl Library {
    /// `steps/` under `$SLOPWATCH_CONFIG_DIR`, or under
    /// `~/.config/slopwatch` when that isn't set.
    pub fn default_dir() -> PathBuf {
        let config = std::env::var_os(CONFIG_DIR_ENV)
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .unwrap_or_default()
                    .join(".config/slopwatch")
            });
        config.join("steps")
    }

    /// Opens the Library at `dir`, creating it if needed. Each preset this
    /// build ships is written once, the first time this Library sees it, so
    /// a fresh Library holds them all, and a preset the developer edited or
    /// deleted stays that way.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let library = Self { dir: dir.into() };
        std::fs::create_dir_all(&library.dir)?;
        let seeded_path = library.dir.join(SEEDED);
        let mut seeded: BTreeSet<String> = match std::fs::read_to_string(&seeded_path) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => return Err(error),
        };
        let fresh: Vec<_> = PRESETS
            .iter()
            .filter(|preset| !seeded.contains(preset.name))
            .collect();
        if fresh.is_empty() {
            return Ok(library);
        }
        for preset in fresh {
            let path = library.path(preset.name);
            if !path.exists() {
                write_atomically(&path, preset.text)?;
            }
            seeded.insert(preset.name.to_owned());
        }
        let list: String = seeded.iter().map(|name| format!("{name}\n")).collect();
        write_atomically(&seeded_path, &list)?;
        Ok(library)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The text of the Library Step `name` as it stands now, or `None` if
    /// there's no such Step.
    pub fn step(&self, name: &str) -> Option<String> {
        if !is_library_step_name(name) {
            return None;
        }
        std::fs::read_to_string(self.path(name)).ok()
    }

    /// Every Library Step, sorted by name, each with the reason it wouldn't
    /// load, if any.
    pub fn list(&self) -> Result<Vec<LibraryStep>, LibraryError> {
        let mut steps = Vec::new();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path
                .extension()
                .is_none_or(|extension| extension != EXTENSION)
            {
                continue;
            }
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if !is_library_step_name(name) {
                continue;
            }
            let text = std::fs::read_to_string(&path)?;
            steps.push(LibraryStep {
                name: name.to_owned(),
                problem: check_library_step(&text).err(),
                text,
            });
        }
        steps.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(steps)
    }

    /// Creates or replaces the Library Step `name`, if `text` would load.
    pub fn save(&self, name: &str, text: &str) -> Result<(), LibraryError> {
        let path = self.checked_path(name)?;
        check_library_step(text).map_err(|message| {
            LibraryError::Invalid(format!("Library Step `{name}` is invalid: {message}"))
        })?;
        write_atomically(&path, text)?;
        Ok(())
    }

    pub fn delete(&self, name: &str) -> Result<(), LibraryError> {
        match std::fs::remove_file(self.checked_path(name)?) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Err(LibraryError::NotFound(name.to_owned()))
            }
            Err(error) => Err(error.into()),
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.{EXTENSION}"))
    }

    fn checked_path(&self, name: &str) -> Result<PathBuf, LibraryError> {
        if is_library_step_name(name) {
            Ok(self.path(name))
        } else {
            Err(LibraryError::BadName(name.to_owned()))
        }
    }
}

/// Writes through a temporary file and a rename, so a Run that loads the
/// Step mid-save reads the old text or the new, never half of one.
fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    file.write_all(text.as_bytes())?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}
