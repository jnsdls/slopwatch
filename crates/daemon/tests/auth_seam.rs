//! Only the auth seam reads the developer's `gh` token, so a GitHub App can
//! replace it in one place.

use std::fs;
use std::path::{Path, PathBuf};

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn only_the_auth_module_runs_gh() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);

    let readers: Vec<_> = files
        .iter()
        .filter(|path| {
            let text = fs::read_to_string(path).unwrap();
            text.contains("\"auth\", \"token\"")
                || text.contains("GH_TOKEN")
                || text.contains("hosts.yml")
        })
        .map(|path| path.strip_prefix(&src).unwrap().to_owned())
        .collect();

    assert_eq!(readers, [PathBuf::from("auth.rs")]);
}
