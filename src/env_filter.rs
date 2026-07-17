//! Child-process environment allowlist — inlined verbatim from the framework's
//! `provider::filter_child_env` (the sole non-corpus intra-workspace dependency
//! of the measurement crate, dropped on extraction). A measured arm spawns
//! `execute_node.py` / `claude -p`; only these variables cross into the child, so
//! an unrelated host secret can never leak into a measured subprocess.

use std::collections::BTreeMap;

const ENV_EXACT: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TMPDIR",
    "TERM",
    "SHLVL",
    "PWD",
    "NODE_PATH",
    "CLAUDE_CODE_USE_BEDROCK",
];

const ENV_PREFIX: &[&str] = &["AWS_", "ANTHROPIC_", "NPM_CONFIG_"];

/// Keep only allowlisted variables from `parent`, preserving values.
pub fn filter_child_env(parent: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    parent
        .iter()
        .filter(|(k, _)| {
            ENV_EXACT.contains(&k.as_str()) || ENV_PREFIX.iter().any(|p| k.starts_with(p))
        })
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
