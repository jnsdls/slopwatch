//! The key a same-SHA Run reuses a settled Outcome under.

use std::fmt::Write as _;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::pipeline::Step;

/// An Outcome is reused only by a Step with the same key: same head SHA,
/// Pipeline id, resolved config and Plugin version.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReuseKey {
    pub head_sha: String,
    pub step: String,
    pub config_hash: String,
    pub plugin_version: String,
}

impl Step {
    /// SHA-256, in hex, of the resolved config: the Plugin name and the
    /// merged `with:`, as JSON with object keys sorted.
    pub fn config_hash(&self) -> String {
        let mut canonical = String::new();
        let config = serde_json::json!({ "plugin": self.plugin, "with": self.config });
        write_canonical(&config, &mut canonical);
        let digest = Sha256::digest(canonical.as_bytes());
        digest.iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    }

    /// `plugin_version` is the one the daemon resolved for this spawn: the
    /// manifest version plus the executable's hash.
    pub fn reuse_key(&self, head_sha: &str, plugin_version: &str) -> ReuseKey {
        ReuseKey {
            head_sha: head_sha.to_owned(),
            step: self.id.clone(),
            config_hash: self.config_hash(),
            plugin_version: plugin_version.to_owned(),
        }
    }
}

/// JSON with object keys sorted at every level, whatever order the map
/// keeps them in.
fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(item, out);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}
