//! The Plugins the daemon can run, and where it finds them.
//!
//! Built-in Plugins run from the daemon's own executable, as
//! `slopwatchd plugin <name> describe` and `slopwatchd plugin <name> run`,
//! so they ship and update with the app and speak the Step contract like
//! any third-party Plugin: `ci`, `claude`, `codex`, `fix`, `human`, `jev`
//! and `merge`.
//!
//! A third-party Plugin is an executable, or a symlink to one, in the
//! Plugins folder: `plugins/<name>` under the config dir (ADR 0012). The
//! daemon looks there at start, on every sync and whenever a client lists
//! Plugins. A file it hasn't seen, or one that changed, gets a locked-down
//! `describe` ([`describe`]), and its manifest is what the Plugin asks
//! for. Whether it may run is the Approval's business (`approvals`), not
//! this module's.

pub mod ci;
pub mod claude;
pub mod codex;
pub mod describe;
pub mod fix;
pub mod human;
pub mod jev;
pub mod merge;
pub mod review;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use serde_json::{Map, Value};
use slopwatch_core::{PluginInfo, Resolver};
use slopwatch_protocol::step::Manifest;
use slopwatch_protocol::{Cli, Flavor, Grant, PluginListing, PluginSettings};

use crate::Library;
use crate::approvals::Approval;
use describe::{DESCRIBE_TIMEOUT, check_manifest, check_name};

/// What a Run's Pipeline can name: the installed Plugins, and Library
/// Steps through `lib/`, read live.
pub struct Plugins {
    /// The executable built-in Plugins run from.
    builtin_exe: PathBuf,
    library: Arc<Library>,
    /// The Plugins folder, once the daemon looks in one.
    dir: Option<PathBuf>,
    /// The `PATH` a `describe` gets, before a Plugin's own settings.
    describe_path: String,
    describe_timeout: Duration,
    /// One scan at a time, so a file isn't described twice at once.
    scanning: tokio::sync::Mutex<()>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// Third-party Plugins that loaded, by name.
    third_party: BTreeMap<String, ThirdParty>,
    /// Files in the Plugins folder that didn't load, by name.
    failed: BTreeMap<String, Failed>,
    /// The developer's settings, by Plugin name.
    settings: HashMap<String, PluginSettings>,
    /// Plugins to describe again on the next scan whatever their file
    /// says, as after their `PATH` setting changed.
    stale: HashSet<String>,
    /// SHA-256 of each executable a version was taken from, and the file
    /// it was taken from.
    hashes: HashMap<PathBuf, (FileStamp, String)>,
}

struct ThirdParty {
    manifest: Manifest,
    program: PathBuf,
    args: Vec<String>,
    /// The file the manifest came from, for a Plugin from the folder.
    /// `None` for one added with [`Plugins::with_plugin`].
    found: Option<FileStamp>,
}

struct Failed {
    path: PathBuf,
    problem: String,
    found: Option<FileStamp>,
}

/// Tells one file from another at a path, or the same file rewritten.
type FileStamp = (u64, u64, u64, Option<SystemTime>);

fn file_stamp(path: &Path) -> std::io::Result<FileStamp> {
    let meta = std::fs::metadata(path)?;
    Ok((meta.dev(), meta.ino(), meta.len(), meta.modified().ok()))
}

/// `plugins/` under the config dir.
pub fn default_dir(flavor: Flavor) -> PathBuf {
    crate::config_dir(flavor).join("plugins")
}

impl Plugins {
    pub fn new(builtin_exe: impl Into<PathBuf>, library: Arc<Library>) -> Self {
        Self {
            builtin_exe: builtin_exe.into(),
            library,
            dir: None,
            describe_path: String::new(),
            describe_timeout: DESCRIBE_TIMEOUT,
            scanning: tokio::sync::Mutex::new(()),
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .expect("no panics while holding the Plugins")
    }

    /// Finds third-party Plugins in `dir` from now on, and looks there
    /// once before returning. Each `describe` gets `path` as its `PATH`,
    /// after the Plugin's own `PATH` setting.
    pub async fn in_folder(mut self, dir: impl Into<PathBuf>, path: impl Into<String>) -> Self {
        self.dir = Some(dir.into());
        self.describe_path = path.into();
        self.scan().await;
        self
    }

    /// How long a `describe` may take. Tests shorten it.
    pub fn with_describe_timeout(mut self, timeout: Duration) -> Self {
        self.describe_timeout = timeout;
        self
    }

    /// Adds a third-party Plugin from outside the Plugins folder, named by
    /// its manifest's id, that runs as `program args`. Tests use it for
    /// Plugins that are shell scripts given as arguments. It needs an
    /// Approval like any other.
    pub fn with_plugin(self, manifest: Manifest, program: PathBuf, args: Vec<String>) -> Self {
        self.state().third_party.insert(
            manifest.id.clone(),
            ThirdParty {
                manifest,
                program,
                args,
                found: None,
            },
        );
        self
    }

    /// The developer's settings for each Plugin, as the store kept them.
    pub fn with_settings(self, settings: HashMap<String, PluginSettings>) -> Self {
        self.state().settings = settings;
        self
    }

    /// Looks in the Plugins folder again: describes each file that's new
    /// or changed, and forgets the ones that went. Returns whether
    /// anything changed.
    pub async fn scan(&self) -> bool {
        let Some(dir) = &self.dir else {
            return false;
        };
        let _scanning = self.scanning.lock().await;
        let files = match list_folder(dir) {
            Ok(files) => files,
            Err(error) => {
                if error.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("slopwatchd: can't read {}: {error}", dir.display());
                }
                Vec::new()
            }
        };
        // What to describe, worked out under the lock and run without it.
        let mut describe = Vec::new();
        let mut changed = false;
        {
            let mut state = self.state();
            let present: HashSet<&String> = files.iter().map(|(name, _)| name).collect();
            let before = state.third_party.len() + state.failed.len();
            state
                .third_party
                .retain(|name, plugin| plugin.found.is_none() || present.contains(name));
            state.failed.retain(|name, _| present.contains(name));
            changed |= state.third_party.len() + state.failed.len() != before;
            for (name, path) in &files {
                let stale = state.stale.remove(name);
                let stamp = file_stamp(path).ok();
                let known = state
                    .third_party
                    .get(name)
                    .map(|plugin| plugin.found)
                    .or_else(|| state.failed.get(name).map(|failed| failed.found));
                if !stale && stamp.is_some() && known == Some(stamp) {
                    continue;
                }
                changed = true;
                let problem = match stamp {
                    None => Some("can't read it; a symlink to nowhere?".to_owned()),
                    Some(_) => check_name(name).or_else(|| not_executable(path)),
                };
                if let Some(problem) = problem {
                    state.third_party.remove(name);
                    let failed = Failed {
                        path: path.clone(),
                        problem,
                        found: stamp,
                    };
                    state.failed.insert(name.clone(), failed);
                    continue;
                }
                let path_env = with_settings_path(state.settings.get(name), &self.describe_path);
                describe.push((name.clone(), path.clone(), stamp, path_env));
            }
        }
        let described = futures_util::future::join_all(describe.into_iter().map(
            |(name, path, stamp, path_env)| async move {
                let manifest = describe::describe(&path, &path_env, self.describe_timeout)
                    .await
                    .and_then(|manifest| match check_manifest(&name, &manifest) {
                        Some(problem) => Err(problem),
                        None => Ok(manifest),
                    });
                (name, path, stamp, manifest)
            },
        ))
        .await;
        let mut state = self.state();
        for (name, path, found, manifest) in described {
            match manifest {
                Ok(manifest) => {
                    state.failed.remove(&name);
                    let plugin = ThirdParty {
                        manifest,
                        program: path,
                        args: vec!["run".to_owned()],
                        found,
                    };
                    state.third_party.insert(name, plugin);
                }
                Err(problem) => {
                    eprintln!("slopwatchd: Plugin `{name}` didn't load: {problem}");
                    state.third_party.remove(&name);
                    let failed = Failed {
                        path,
                        problem,
                        found,
                    };
                    state.failed.insert(name, failed);
                }
            }
        }
        changed
    }

    pub fn manifest(&self, plugin: &str) -> Option<Manifest> {
        builtin_manifest(plugin).or_else(|| {
            self.state()
                .third_party
                .get(plugin)
                .map(|plugin| plugin.manifest.clone())
        })
    }

    /// The manifest of the third-party Plugin `plugin`, for approving it,
    /// or why it can't be approved.
    pub fn approvable(&self, plugin: &str) -> Result<Manifest, NotApprovable> {
        if builtin_manifest(plugin).is_some() {
            return Err(NotApprovable::Builtin);
        }
        let state = self.state();
        if let Some(found) = state.third_party.get(plugin) {
            return Ok(found.manifest.clone());
        }
        match state.failed.get(plugin) {
            Some(failed) => Err(NotApprovable::Failed(failed.problem.clone())),
            None => Err(NotApprovable::Unknown),
        }
    }

    /// Whether the daemon knows a Plugin by this name, loaded or not.
    pub fn knows(&self, plugin: &str) -> bool {
        let state = self.state();
        builtin_manifest(plugin).is_some()
            || state.third_party.contains_key(plugin)
            || state.failed.contains_key(plugin)
    }

    /// The version an Outcome is reused under: the manifest version plus
    /// a hash of the executable as it is now (ADR 0012). A symlink is
    /// followed, so a rebuild behind it counts. The hash is taken again
    /// whenever the file changed. `None` when the Plugin isn't installed
    /// or its executable can't be read, and then nothing is reused.
    pub fn version(&self, plugin: &str) -> Option<String> {
        let (version, program) = match builtin_manifest(plugin) {
            Some(manifest) => (manifest.version, self.builtin_exe.clone()),
            None => {
                let state = self.state();
                let plugin = state.third_party.get(plugin)?;
                (plugin.manifest.version.clone(), plugin.program.clone())
            }
        };
        let hash = self.hash(&program)?;
        Some(format!("{version}+{hash}"))
    }

    fn hash(&self, program: &Path) -> Option<String> {
        let resolved = std::fs::canonicalize(program)
            .inspect_err(|error| eprintln!("slopwatchd: can't hash {}: {error}", program.display()))
            .ok()?;
        let stamp = file_stamp(&resolved)
            .inspect_err(|error| {
                eprintln!("slopwatchd: can't hash {}: {error}", resolved.display())
            })
            .ok()?;
        if let Some((at, hash)) = self.state().hashes.get(&resolved)
            && *at == stamp
        {
            return Some(hash.clone());
        }
        let hash = file_hash(&resolved)?;
        self.state().hashes.insert(resolved, (stamp, hash.clone()));
        Some(hash)
    }

    /// The manifests of the Plugins that ship with the app.
    pub fn builtin_manifests(&self) -> Vec<Manifest> {
        builtins()
    }

    /// The program and arguments that start a session with `plugin`.
    pub fn command(&self, plugin: &str) -> Option<(PathBuf, Vec<String>)> {
        if builtin_manifest(plugin).is_some() {
            return Some((
                self.builtin_exe.clone(),
                vec!["plugin".into(), plugin.into(), "run".into()],
            ));
        }
        let state = self.state();
        let plugin = state.third_party.get(plugin)?;
        Some((plugin.program.clone(), plugin.args.clone()))
    }

    /// How many of the Plugin's Steps may run at once: the developer's
    /// cap, or else the manifest's. `None` leaves only the global cap.
    pub fn cap(&self, plugin: &str) -> Option<u32> {
        let set = self
            .state()
            .settings
            .get(plugin)
            .and_then(|settings| settings.cap);
        set.or_else(|| self.manifest(plugin)?.concurrency)
    }

    /// The `PATH` the Plugin's Steps get: its own setting in front of
    /// `base`.
    pub fn path(&self, plugin: &str, base: &str) -> String {
        with_settings_path(self.state().settings.get(plugin), base)
    }

    /// The config directory the developer set for the Plugin's CLI, if any.
    pub fn config_dir(&self, plugin: &str) -> Option<String> {
        self.state().settings.get(plugin)?.config_dir.clone()
    }

    /// Replaces the developer's settings for `plugin`. A third-party
    /// Plugin is described again on the next scan, since its `PATH` may
    /// be what it lacked.
    pub fn set_settings(&self, plugin: &str, settings: PluginSettings) {
        let mut state = self.state();
        state.stale.insert(plugin.to_owned());
        if settings == PluginSettings::default() {
            state.settings.remove(plugin);
        } else {
            state.settings.insert(plugin.to_owned(), settings);
        }
    }

    /// Every Plugin as the Plugins list shows it, sorted by name. A file
    /// with a built-in's name is listed after the built-in, with why it
    /// didn't load.
    pub fn listings(&self, approvals: &[Approval]) -> Vec<PluginListing> {
        let approval = |name: &str| approvals.iter().find(|approval| approval.plugin == name);
        let state = self.state();
        let settings = |name: &str| state.settings.get(name).cloned().unwrap_or_default();
        let mut listings = Vec::new();
        let loaded = |name: &str, manifest: &Manifest, builtin, path: Option<&Path>| {
            let approval = approval(name);
            PluginListing {
                name: name.to_owned(),
                builtin,
                path: path.map(|path| path.display().to_string()),
                version: Some(manifest.version.clone()),
                asks: Some(Grant::of(manifest)),
                approved: approval.map(|approval| approval.grant.clone()),
                approved_at: approval.map(|approval| approval.approved_at),
                problem: None,
                settings: settings(name),
            }
        };
        for manifest in builtins() {
            let listing = loaded(&manifest.id, &manifest, true, None);
            listings.push(listing);
        }
        for (name, plugin) in &state.third_party {
            let path = plugin.found.map(|_| plugin.program.as_path());
            let listing = loaded(name, &plugin.manifest, false, path);
            listings.push(listing);
        }
        for (name, failed) in &state.failed {
            // A built-in's Approval and settings aren't this file's.
            let reserved = builtin_manifest(name).is_some();
            let approval = approval(name).filter(|_| !reserved);
            listings.push(PluginListing {
                name: name.clone(),
                builtin: false,
                path: Some(failed.path.display().to_string()),
                version: None,
                asks: None,
                approved: approval.map(|approval| approval.grant.clone()),
                approved_at: approval.map(|approval| approval.approved_at),
                problem: Some(failed.problem.clone()),
                settings: if reserved {
                    PluginSettings::default()
                } else {
                    settings(name)
                },
            });
        }
        listings.sort_by(|a, b| (&a.name, !a.builtin).cmp(&(&b.name, !b.builtin)));
        listings
    }
}

/// The agent CLI a built-in Plugin's Step runs, by its `with:`: Claude or
/// Codex for their own Plugins, and the one `fix` names in `agent`.
pub fn agent_cli(plugin: &str, with: &Map<String, Value>) -> Option<Cli> {
    match plugin {
        "claude" => Some(Cli::Claude),
        "codex" => Some(Cli::Codex),
        fix::ID => Some(match with.get("agent").and_then(Value::as_str) {
            Some("codex") => Cli::Codex,
            _ => Cli::Claude,
        }),
        _ => None,
    }
}

/// Why a Plugin can't be approved.
#[derive(Debug, PartialEq, Eq)]
pub enum NotApprovable {
    /// It ships with the app, approved.
    Builtin,
    /// It didn't load, for this reason.
    Failed(String),
    /// No Plugin has the name.
    Unknown,
}

/// The files in the Plugins folder, by name. Hidden files are left out.
fn list_folder(dir: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        files.push((name, entry.path()));
    }
    Ok(files)
}

fn not_executable(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return Some("isn't a file".to_owned());
    }
    (meta.permissions().mode() & 0o111 == 0).then(|| "isn't executable; `chmod +x` it".to_owned())
}

fn with_settings_path(settings: Option<&PluginSettings>, base: &str) -> String {
    let own = settings.map_or(&[][..], |settings| &settings.path[..]);
    own.iter()
        .map(String::as_str)
        .chain((!base.is_empty()).then_some(base))
        .collect::<Vec<_>>()
        .join(":")
}

fn file_hash(path: &Path) -> Option<String> {
    let mut hasher = Sha256::new();
    let read = std::fs::File::open(path).and_then(|mut file| {
        let mut buffer = vec![0; 1 << 16];
        loop {
            match file.read(&mut buffer)? {
                0 => return Ok(()),
                n => hasher.update(&buffer[..n]),
            }
        }
    });
    if let Err(error) = read {
        eprintln!("slopwatchd: can't hash {}: {error}", path.display());
        return None;
    }
    Some(
        hasher
            .finalize()
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            }),
    )
}

/// The manifests of the Plugins that ship with the app.
fn builtins() -> Vec<Manifest> {
    vec![
        ci::manifest(),
        claude::manifest(),
        codex::manifest(),
        fix::manifest(),
        jev::manifest(),
        merge::manifest(),
        human::manifest(),
    ]
}

fn builtin_manifest(plugin: &str) -> Option<Manifest> {
    builtins()
        .into_iter()
        .find(|manifest| manifest.id == plugin)
}

impl Resolver for Plugins {
    fn library_step(&self, name: &str) -> Option<String> {
        self.library.step(name)
    }

    fn plugin(&self, name: &str) -> Option<PluginInfo> {
        self.manifest(name).map(|manifest| PluginInfo {
            workspace: manifest.workspace,
            builtin: builtin_manifest(name).is_some(),
        })
    }
}

/// Serves `slopwatchd plugin <name> describe|run`. `args` are the ones
/// after `plugin`.
pub fn main(args: &[String]) -> ExitCode {
    let (Some(name), Some(verb)) = (args.first(), args.get(1)) else {
        eprintln!("usage: slopwatchd plugin <name> describe|run");
        return ExitCode::from(2);
    };
    let Some(manifest) = builtin_manifest(name) else {
        eprintln!("slopwatchd: no built-in Plugin named `{name}`");
        return ExitCode::from(2);
    };
    let result = match verb.as_str() {
        "describe" => {
            let text = serde_json::to_string(&manifest).expect("manifests always serialize");
            writeln!(std::io::stdout(), "{text}")
        }
        "run" => match name.as_str() {
            "ci" => ci::run(std::io::stdin().lock(), std::io::stdout().lock()),
            "merge" => merge::run(std::io::stdin().lock(), std::io::stdout().lock()),
            "human" => human::run(std::io::stdin().lock(), std::io::stdout().lock()),
            "claude" => claude::run(),
            "codex" => codex::run(),
            "fix" => fix::run(),
            "jev" => jev::run(
                std::io::BufReader::new(std::io::stdin()),
                std::io::stdout().lock(),
            ),
            _ => unreachable!("every built-in manifest has a session"),
        },
        _ => {
            eprintln!("usage: slopwatchd plugin <name> describe|run");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("slopwatchd plugin {name}: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugins(dir: &Path, exe: &Path) -> Plugins {
        let library = Arc::new(Library::open(dir.join("steps")).unwrap());
        Plugins::new(exe, library)
    }

    fn write_plugin(dir: &Path, name: &str, manifest: &str) -> PathBuf {
        let path = dir.join(name);
        // Written next to it and renamed in, as a build swaps a file.
        let next = dir.join(format!(".{name}.next"));
        std::fs::write(&next, format!("#!/bin/sh\necho '{manifest}'\n")).unwrap();
        std::fs::set_permissions(&next, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::rename(&next, &path).unwrap();
        path
    }

    fn manifest(id: &str, version: &str) -> String {
        format!(r#"{{"id":"{id}","version":"{version}","dialect":1,"workspace":"none"}}"#)
    }

    #[test]
    fn the_version_follows_the_executable_and_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("slopwatchd");
        std::fs::write(&exe, "build one").unwrap();
        let plugins = plugins(dir.path(), &exe);

        let first = plugins.version("ci").unwrap();
        assert!(first.starts_with(&format!("{}+", ci::manifest().version)));
        assert_eq!(plugins.version("ci").unwrap(), first);
        assert_eq!(plugins.version("nobody"), None);

        // An install swaps in a new file.
        let next = dir.path().join("next");
        std::fs::write(&next, "build two").unwrap();
        std::fs::rename(&next, &exe).unwrap();
        assert_ne!(plugins.version("ci").unwrap(), first);

        std::fs::remove_file(&exe).unwrap();
        assert_eq!(plugins.version("ci"), None);
    }

    #[tokio::test]
    async fn the_folder_holds_plugins_by_file_name_and_a_rebuild_changes_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("plugins");
        std::fs::create_dir(&folder).unwrap();
        write_plugin(&folder, "lint", &manifest("lint", "1"));
        let plugins = plugins(dir.path(), &dir.path().join("slopwatchd"))
            .in_folder(&folder, "/usr/bin:/bin")
            .await;

        assert_eq!(plugins.manifest("lint").unwrap().version, "1");
        let (program, args) = plugins.command("lint").unwrap();
        assert_eq!(
            (program, args),
            (folder.join("lint"), vec!["run".to_owned()])
        );
        let first = plugins.version("lint").unwrap();
        assert!(first.starts_with("1+"), "{first}");
        assert!(!plugins.scan().await, "nothing changed");

        // Same manifest, new build.
        let path = write_plugin(&folder, "lint", &manifest("lint", "1"));
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"# rebuilt\n")
            .unwrap();
        assert_ne!(
            plugins.version("lint").unwrap(),
            first,
            "re-hashed at spawn"
        );
        assert!(plugins.scan().await);
        assert_eq!(plugins.manifest("lint").unwrap().version, "1");

        std::fs::remove_file(&path).unwrap();
        assert!(plugins.scan().await);
        assert_eq!(plugins.manifest("lint"), None);
    }

    #[tokio::test]
    async fn a_symlink_is_followed_and_its_target_is_hashed() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("plugins");
        let build = dir.path().join("build");
        std::fs::create_dir(&folder).unwrap();
        std::fs::create_dir(&build).unwrap();
        write_plugin(&build, "lint", &manifest("lint", "1"));
        std::os::unix::fs::symlink(build.join("lint"), folder.join("lint")).unwrap();
        let plugins = plugins(dir.path(), &dir.path().join("slopwatchd"))
            .in_folder(&folder, "/usr/bin:/bin")
            .await;
        let first = plugins.version("lint").unwrap();

        std::fs::OpenOptions::new()
            .append(true)
            .open(build.join("lint"))
            .unwrap()
            .write_all(b"# rebuilt\n")
            .unwrap();
        assert_ne!(plugins.version("lint").unwrap(), first);
    }

    #[tokio::test]
    async fn files_that_cant_be_plugins_are_listed_with_why() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("plugins");
        std::fs::create_dir(&folder).unwrap();
        // A reserved name never runs: this one would fail the test if it
        // did, by leaving a file behind.
        let ran = dir.path().join("ran");
        let merge = folder.join("merge");
        std::fs::write(&merge, format!("#!/bin/sh\ntouch '{}'\n", ran.display())).unwrap();
        std::fs::set_permissions(&merge, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(folder.join("plain"), "not executable").unwrap();
        write_plugin(&folder, "other", &manifest("lint", "1"));
        std::os::unix::fs::symlink(dir.path().join("nowhere"), folder.join("dangling")).unwrap();
        std::fs::write(folder.join(".hidden"), "").unwrap();
        let plugins = plugins(dir.path(), &dir.path().join("slopwatchd"))
            .in_folder(&folder, "/usr/bin:/bin")
            .await;

        let listed = plugins.listings(&[]);
        let problem = |name: &str| {
            listed
                .iter()
                .find(|listing| listing.name == name && !listing.builtin)
                .and_then(|listing| listing.problem.clone())
                .unwrap_or_default()
        };
        assert!(problem("merge").contains("reserved"), "{listed:?}");
        assert!(problem("plain").contains("executable"));
        assert!(problem("other").contains("`lint`"));
        assert!(problem("dangling").contains("symlink"));
        assert!(!listed.iter().any(|listing| listing.name == ".hidden"));
        assert!(!ran.exists(), "a reserved name ran");
        // The built-in `merge` is still the one Pipelines get.
        assert!(plugins.plugin("merge").unwrap().builtin);
        assert_eq!(plugins.plugin("other"), None);
    }

    #[tokio::test]
    async fn settings_put_the_plugins_dirs_first_and_cap_it() {
        let dir = tempfile::tempdir().unwrap();
        let tools = dir.path().join("tools");
        let folder = dir.path().join("plugins");
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::create_dir(&folder).unwrap();
        // `describe` runs only once the tool on its setting's PATH is there.
        std::fs::write(
            tools.join("say-manifest"),
            format!("#!/bin/sh\necho '{}'\n", manifest("lint", "1")),
        )
        .unwrap();
        std::fs::set_permissions(
            tools.join("say-manifest"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let lint = folder.join("lint");
        std::fs::write(&lint, "#!/bin/sh\nexec say-manifest\n").unwrap();
        std::fs::set_permissions(&lint, std::fs::Permissions::from_mode(0o755)).unwrap();
        let plugins = plugins(dir.path(), &dir.path().join("slopwatchd"))
            .in_folder(&folder, "/usr/bin:/bin")
            .await;
        assert_eq!(plugins.manifest("lint"), None);

        let tools = tools.to_str().unwrap().to_owned();
        plugins.set_settings(
            "lint",
            PluginSettings {
                path: vec![tools.clone()],
                cap: Some(2),
                config_dir: None,
            },
        );
        assert!(plugins.scan().await);
        assert!(plugins.manifest("lint").is_some());
        assert_eq!(plugins.cap("lint"), Some(2));
        assert_eq!(
            plugins.path("lint", "/usr/bin"),
            format!("{tools}:/usr/bin")
        );
        assert_eq!(plugins.path("ci", "/usr/bin"), "/usr/bin");
    }

    #[test]
    fn a_fix_step_runs_the_cli_of_the_agent_it_names() {
        use slopwatch_protocol::AGENT_PLUGINS;
        let with = |value: serde_json::Value| value.as_object().unwrap().clone();
        let none = Map::new();
        assert_eq!(agent_cli("claude", &none), Some(Cli::Claude));
        assert_eq!(agent_cli("codex", &none), Some(Cli::Codex));
        assert_eq!(agent_cli("fix", &none), Some(Cli::Claude));
        let codex = with(serde_json::json!({ "agent": "codex" }));
        assert_eq!(agent_cli("fix", &codex), Some(Cli::Codex));
        assert_eq!(agent_cli("ci", &none), None);
        assert_eq!(agent_cli("lint", &none), None);
        for plugin in AGENT_PLUGINS {
            assert!(agent_cli(plugin, &none).is_some(), "{plugin}");
        }
    }
}
