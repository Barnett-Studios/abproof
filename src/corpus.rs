//! Corpus loading — reads RED-baseline node fixtures from disk and bridges them to
//! per-arm NodeJson payloads consumed by `execute_node.py`.

use crate::experiment::{ArmConfig, ContextStrategy};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Node metadata parsed from `meta.yaml` in the node fixture directory.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct NodeMeta {
    pub id: String,
    pub language: String,
    /// Editable files relative to the node working directory.
    pub files: Vec<String>,
    /// Shell command run inside the node working directory to validate output.
    pub accept: String,
    #[serde(default)]
    pub forbid: Vec<String>,
    /// Task description shown to the model. When absent, a generic instruction is
    /// synthesized from the primary file's RED content (legacy stub-based nodes).
    #[serde(default)]
    pub change: Option<String>,
    /// Toolchain gate: executables that must be present on PATH to run this node.
    /// Empty list means no gate (python nodes carry `[]`).  When any listed tool
    /// is absent the node is SKIPPED — never scored as a failure.
    #[serde(default)]
    pub requires: Vec<String>,
}

/// A fully-loaded corpus node — metadata plus the complete RED seed project.
#[derive(Debug, Clone)]
pub struct CorpusNode {
    pub meta: NodeMeta,
    /// Absolute path to `measurement/corpus/red-baseline/<id>/`.
    pub dir: PathBuf,
    /// The complete RED project as `(relative path, content)`: stub source(s),
    /// acceptance test(s), and any scaffold (`Cargo.toml`, `go.mod`, `src/` layout).
    /// Loaded from a `seed/` subdir if present, else synthesized from the legacy
    /// `stub.<ext>` + `acceptance_test.<ext>` pair.
    pub seed: Vec<(String, String)>,
    /// Contents of `context.md` if present; `None` otherwise.
    pub context: Option<String>,
}

/// The exact object `execute_node.py` reads from argv\[1\]: `{id, change, files, accept, forbid?}`.
/// `root` is supplied out-of-band via `EXECUTE_NODE_ROOT` (Task 5), NOT inside this JSON.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct NodeJson {
    pub id: String,
    pub change: String,
    pub files: Vec<String>,
    pub accept: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub forbid: Vec<String>,
    /// In-memory only — the RED files + acceptance test to materialize into a clean
    /// git work tree for this run. `#[serde(skip)]`: never written to the JSON
    /// `execute_node.py` reads, so the contract stays byte-identical. `None` means
    /// the caller supplies `EXECUTE_NODE_ROOT` out-of-band (tests with a prebuilt tree).
    #[serde(skip)]
    pub materialize: Option<crate::worktree::MaterializeSpec>,
    /// Toolchain gate — mirrors `NodeMeta::requires`.  `#[serde(skip)]`: never
    /// written to the JSON contract; the driver reads this before spawning python.
    #[serde(skip)]
    pub requires: Vec<String>,
}

#[derive(Debug, Error)]
pub enum CorpusError {
    #[error("{0}: {1}")]
    Io(PathBuf, String),
    #[error("node '{0}': meta.yaml: {1}")]
    Meta(String, String),
    #[error("battery glob '{0}' matched no nodes under {1}")]
    EmptyGlob(String, PathBuf),
}

/// Absolute path to the RED-baseline corpus root shipped with the framework.
/// Resolve the RED-baseline corpus root. `$ABPROOF_CORPUS` wins if set; otherwise
/// walk up from the current directory looking for `measurement/corpus/red-baseline`
/// (so it resolves inside a checkout without configuration), then fall back to that
/// relative path. Standalone deployments set `$ABPROOF_CORPUS`.
pub fn red_baseline_root() -> PathBuf {
    if let Ok(p) = std::env::var("ABPROOF_CORPUS") {
        return PathBuf::from(p);
    }
    if let Ok(cwd) = std::env::current_dir() {
        let mut dir = cwd.as_path();
        loop {
            let candidate = dir.join("measurement/corpus/red-baseline");
            if candidate.is_dir() {
                return candidate;
            }
            match dir.parent() {
                Some(p) => dir = p,
                None => break,
            }
        }
    }
    PathBuf::from("measurement/corpus/red-baseline")
}

/// Load a single corpus node from `dir`.
///
/// Reads `meta.yaml`, then the RED seed project: either a `seed/` subdir (the whole
/// project, any layout — used for scaffolded languages like Rust/Go) or the legacy
/// `stub.<ext>` + `acceptance_test.<ext>` pair (flat single-file Python nodes).
/// Optionally reads `context.md`.
pub fn load_node(dir: &Path) -> Result<CorpusNode, CorpusError> {
    let meta_path = dir.join("meta.yaml");
    let meta_content = std::fs::read_to_string(&meta_path)
        .map_err(|e| CorpusError::Io(meta_path.clone(), e.to_string()))?;
    let meta: NodeMeta = serde_yaml::from_str(&meta_content).map_err(|e| {
        let node_id = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        CorpusError::Meta(node_id, e.to_string())
    })?;

    let seed_dir = dir.join("seed");
    let seed = if seed_dir.is_dir() {
        let mut files = Vec::new();
        collect_seed_files(&seed_dir, &seed_dir, &mut files)?;
        files.sort(); // deterministic order regardless of readdir
        files
    } else {
        // Legacy: primary file gets the stub content; the acceptance test its own name.
        let (_, stub) = find_stem_file(dir, "stub")?;
        let (acceptance_name, acceptance) = find_stem_file(dir, "acceptance_test")?;
        let primary =
            meta.files.first().cloned().ok_or_else(|| {
                CorpusError::Meta(meta.id.clone(), "meta.files is empty".to_string())
            })?;
        vec![(primary, stub), (acceptance_name, acceptance)]
    };

    let context_path = dir.join("context.md");
    let context = if context_path.exists() {
        Some(
            std::fs::read_to_string(&context_path)
                .map_err(|e| CorpusError::Io(context_path.clone(), e.to_string()))?,
        )
    } else {
        None
    };

    Ok(CorpusNode {
        meta,
        dir: dir.to_path_buf(),
        seed,
        context,
    })
}

/// Recursively collect every file under `dir` as `(path relative to `base`, content)`.
fn collect_seed_files(
    dir: &Path,
    base: &Path,
    out: &mut Vec<(String, String)>,
) -> Result<(), CorpusError> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| CorpusError::Io(dir.to_path_buf(), e.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|e| CorpusError::Io(dir.to_path_buf(), e.to_string()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|e| CorpusError::Io(path.clone(), e.to_string()))?;
        if file_type.is_dir() {
            collect_seed_files(&path, base, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(base)
                .map_err(|e| CorpusError::Io(path.clone(), e.to_string()))?
                .to_str()
                .ok_or_else(|| CorpusError::Io(path.clone(), "non-utf8 path".to_string()))?
                .to_string();
            let content = std::fs::read_to_string(&path)
                .map_err(|e| CorpusError::Io(path.clone(), e.to_string()))?;
            out.push((rel, content));
        }
    }
    Ok(())
}

/// Scan `dir` for a file whose stem equals `stem` (any extension). Returns
/// `(filename, contents)` — the filename is needed to materialize the acceptance
/// test under the exact name its `accept` command invokes.
///
/// Candidates are sorted before selection so the result is deterministic regardless
/// of the underlying filesystem readdir order.
fn find_stem_file(dir: &Path, stem: &str) -> Result<(String, String), CorpusError> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| CorpusError::Io(dir.to_path_buf(), e.to_string()))?;

    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| CorpusError::Io(dir.to_path_buf(), e.to_string()))?;
        let path = entry.path();
        if path.file_stem().and_then(|s| s.to_str()) == Some(stem) && path.extension().is_some() {
            candidates.push(path);
        }
    }
    candidates.sort();

    if let Some(path) = candidates.into_iter().next() {
        let content = std::fs::read_to_string(&path)
            .map_err(|e| CorpusError::Io(path.clone(), e.to_string()))?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .ok_or_else(|| CorpusError::Io(path.clone(), "non-utf8 filename".to_string()))?
            .to_string();
        Ok((name, content))
    } else {
        Err(CorpusError::Io(
            dir.join(stem),
            format!("no file with stem '{stem}' found"),
        ))
    }
}

/// Load all nodes matching `patterns` under `root`.
///
/// Pattern semantics (no external glob crate):
/// - Trailing `/*` — match every immediate child directory of `root/<prefix>`.
/// - Exact name — match the single directory `root/<name>`.
///
/// Returns `EmptyGlob` when a pattern produces zero matches.
pub fn load_battery(root: &Path, patterns: &[String]) -> Result<Vec<CorpusNode>, CorpusError> {
    let mut nodes = Vec::new();
    for pattern in patterns {
        let dirs = battery_matches(root, pattern)?;
        if dirs.is_empty() {
            return Err(CorpusError::EmptyGlob(pattern.clone(), root.to_path_buf()));
        }
        for dir in dirs {
            nodes.push(load_node(&dir)?);
        }
    }
    Ok(nodes)
}

/// Resolve a battery pattern against `root` → sorted list of matching node directories.
fn battery_matches(root: &Path, pattern: &str) -> Result<Vec<PathBuf>, CorpusError> {
    if let Some(prefix) = pattern.strip_suffix("/*") {
        let search_root = root.join(prefix);
        if !search_root.is_dir() {
            return Ok(vec![]);
        }
        let mut dirs = Vec::new();
        let entries = std::fs::read_dir(&search_root)
            .map_err(|e| CorpusError::Io(search_root.clone(), e.to_string()))?;
        for entry in entries {
            let entry = entry.map_err(|e| CorpusError::Io(search_root.clone(), e.to_string()))?;
            if entry.path().is_dir() {
                dirs.push(entry.path());
            }
        }
        dirs.sort();
        Ok(dirs)
    } else {
        let candidate = root.join(pattern);
        if candidate.is_dir() {
            Ok(vec![candidate])
        } else {
            Ok(vec![])
        }
    }
}

/// Produce the `NodeJson` payload for `arm`, applying the context knob.
///
/// `change` is the node's own `meta.change` when present (ported multi-language
/// nodes carry a specific task description); otherwise a generic instruction is
/// synthesized from the primary file's RED content (legacy stub nodes — the model
/// also sees the full file via `build_prompt`). When `arm.context` is `Cxpak` and
/// the node has a committed `context.md`, it is appended under `## Context`.
pub fn bridge_node(node: &CorpusNode, arm: &ArmConfig) -> NodeJson {
    let files_str = node.meta.files.join(", ");
    let mut change = match &node.meta.change {
        Some(c) => c.clone(),
        None => {
            let primary_content = node
                .meta
                .files
                .first()
                .and_then(|f| node.seed.iter().find(|(p, _)| p == f))
                .map(|(_, c)| c.trim_end().to_string())
                .unwrap_or_default();
            format!(
                "Implement the function in {files_str} so the acceptance test passes.\n\n```\n{primary_content}\n```",
            )
        }
    };
    if arm.context == ContextStrategy::Cxpak {
        if let Some(ctx) = &node.context {
            change.push_str(&format!("\n\n## Context\n{ctx}"));
        }
    }
    // Materialize the entire RED seed project. `None` only if the node has no seed.
    let materialize = if node.seed.is_empty() {
        None
    } else {
        Some(crate::worktree::MaterializeSpec {
            files: node.seed.clone(),
        })
    };
    NodeJson {
        id: node.meta.id.clone(),
        change,
        files: node.meta.files.clone(),
        accept: node.meta.accept.clone(),
        forbid: node.meta.forbid.clone(),
        requires: node.meta.requires.clone(),
        materialize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::experiment::{Backend, ContextStrategy};
    use std::collections::BTreeMap;

    /// Corpus root for the UNIT tests: a vendored, self-contained 2-node fixture
    /// (`py-add`, `cpp-all-your-base`) so `cargo test` runs standalone, decoupled from
    /// the full corpus. The production resolver `fixture_root()` (ABPROOF_CORPUS /
    /// walk-up) is what real `abproof run` invocations use.
    fn fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus-fixture/red-baseline")
    }

    fn none_arm() -> ArmConfig {
        ArmConfig {
            loop_name: "execute-node".into(),
            model: "local-default".into(),
            context: ContextStrategy::None,
            env: BTreeMap::default(),
            backend: Backend::Local,
        }
    }

    fn cxpak_arm() -> ArmConfig {
        ArmConfig {
            loop_name: "execute-node".into(),
            model: "local-default".into(),
            context: ContextStrategy::Cxpak,
            env: BTreeMap::default(),
            backend: Backend::Local,
        }
    }

    #[test]
    fn loads_py_add_node() {
        let n = load_node(&fixture_root().join("py-add")).expect("load");
        assert_eq!(n.meta.id, "py-add");
        assert_eq!(n.meta.files, vec!["calc.py"]);
        assert!(
            n.seed
                .iter()
                .any(|(p, c)| p == "calc.py" && c.contains("NotImplementedError")),
            "seed must hold the RED calc.py stub"
        );
        assert!(
            n.seed.iter().any(|(p, _)| p == "acceptance_test.py"),
            "seed must hold the acceptance test"
        );
        assert!(n.context.is_some());
    }

    #[test]
    fn bridge_none_omits_context() {
        let n = load_node(&fixture_root().join("py-add")).unwrap();
        let j = bridge_node(&n, &none_arm());
        assert_eq!(j.id, "py-add");
        assert_eq!(j.files, vec!["calc.py"]);
        assert_eq!(j.accept, "python3 acceptance_test.py");
        assert!(
            !j.change.contains("## Context"),
            "none arm must not inject context"
        );
    }

    #[test]
    fn bridge_cxpak_injects_context() {
        let n = load_node(&fixture_root().join("py-add")).unwrap();
        let j = bridge_node(&n, &cxpak_arm());
        assert!(
            j.change.contains("## Context"),
            "cxpak arm injects committed context.md"
        );
        assert!(j.change.contains(n.context.as_deref().unwrap()));
    }

    #[test]
    fn bridge_output_matches_execute_node_schema() {
        // CONTRACT: serialize the bridge output, then deserialize under a struct mirroring
        // exactly the keys execute_node.py reads (node["change"], node["files"],
        // node["accept"], node.get("id"), node.get("forbid", [])). If the script's
        // schema drifts, this fails loudly.
        #[derive(serde::Deserialize)]
        struct ExecNodeReads {
            id: String,
            change: String,
            files: Vec<String>,
            accept: String,
            #[serde(default)]
            forbid: Vec<String>,
        }
        let n = load_node(&fixture_root().join("py-add")).unwrap();
        let wire = serde_json::to_string(&bridge_node(&n, &none_arm())).unwrap();
        let read: ExecNodeReads = serde_json::from_str(&wire).expect("execute_node.py schema");
        assert_eq!(read.id, n.meta.id);
        assert_eq!(read.files, n.meta.files);
        assert_eq!(read.accept, n.meta.accept);
        assert!(!read.change.is_empty());
        assert!(read.forbid.is_empty());
    }

    #[test]
    fn load_battery_exact_name() {
        let root = fixture_root();
        let patterns = vec!["py-add".to_string()];
        let nodes = load_battery(&root, &patterns).expect("battery");
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].meta.id, "py-add");
    }

    #[test]
    fn load_battery_empty_glob_errors() {
        let root = fixture_root();
        let patterns = vec!["nonexistent-node".to_string()];
        let err = load_battery(&root, &patterns).unwrap_err();
        assert!(matches!(err, CorpusError::EmptyGlob(_, _)));
    }

    #[test]
    fn forbid_skipped_when_empty() {
        let n = load_node(&fixture_root().join("py-add")).unwrap();
        let j = bridge_node(&n, &none_arm());
        let wire = serde_json::to_string(&j).unwrap();
        // skip_serializing_if = Vec::is_empty → "forbid" key absent from JSON
        assert!(
            !wire.contains("forbid"),
            "empty forbid must be absent from JSON"
        );
    }

    #[test]
    fn requires_deserialises_from_meta_yaml() {
        // cpp nodes carry `requires: ["cmake"]`; this must land on NodeMeta.requires.
        let n = load_node(&fixture_root().join("cpp-all-your-base")).expect("load cpp node");
        assert_eq!(
            n.meta.requires,
            vec!["cmake"],
            "cpp node must carry requires=[cmake]"
        );

        // py-add has no requires field in meta.yaml → defaults to empty vec.
        let py = load_node(&fixture_root().join("py-add")).expect("load py node");
        assert!(
            py.meta.requires.is_empty(),
            "py node with no requires field must default to empty vec"
        );
    }

    #[test]
    fn requires_threaded_to_node_json_and_not_serialised() {
        // bridge_node must copy requires onto NodeJson.
        let n = load_node(&fixture_root().join("cpp-all-your-base")).expect("load");
        let j = bridge_node(&n, &none_arm());
        assert_eq!(j.requires, vec!["cmake"], "bridge_node must copy requires");

        // But `requires` is #[serde(skip)] → must be absent from the JSON wire format.
        let wire = serde_json::to_string(&j).unwrap();
        assert!(
            !wire.contains("requires"),
            "requires must be absent from JSON (execute_node.py contract); wire={wire}"
        );
    }
}
