//! Walks the full RED-baseline corpus (not the 2-node `tests/corpus-fixture`) through
//! `load_node`, with an exact expected count.
//!
//! abproof#32: 47 of 250 real nodes — every Exercism java node, which ships a Gradle
//! wrapper jar — could not be loaded at all, and nothing in this repo's CI walked the
//! real corpus to catch it: `tests/cli_dispatch.rs` uses the small fixture, and
//! `tests/live_smoke.rs` reaches `red_baseline_root()` only through named nodes. This
//! test is the one that makes that class impossible to reintroduce.
//!
//! CI (`.github/workflows/ci.yml`) does not vendor the real corpus — only the 2-node
//! fixture — so this test resolves it the same way `abproof::corpus::red_baseline_root`
//! does: `$ABPROOF_CORPUS` first, else the sibling `../corpus/red-baseline` checkout this
//! factory's repo layout uses. When neither exists the test reports why it is skipping
//! and passes — but loudly, with `eprintln!`, never by quietly matching zero nodes.

use std::path::PathBuf;

/// The real corpus currently ships exactly 250 nodes (abproof#32's own measurement).
/// A test with no expected count would pass just as happily over 10 nodes as 250;
/// this is what the acceptance criteria asks for ("an exact expected count").
const EXPECTED_NODE_COUNT: usize = 250;

fn resolve_real_corpus_root() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("ABPROOF_CORPUS") {
        let p = PathBuf::from(p);
        return p.is_dir().then_some(p);
    }
    let sibling = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../corpus/red-baseline");
    sibling.is_dir().then_some(sibling)
}

#[test]
fn every_real_corpus_node_loads() {
    let root = match resolve_real_corpus_root() {
        Some(r) => r,
        None => {
            eprintln!(
                "SKIP every_real_corpus_node_loads: real corpus not found. Set $ABPROOF_CORPUS \
                 to a red-baseline checkout, or run from a tree with ../corpus checked out \
                 alongside abproof, to exercise this test."
            );
            return;
        }
    };

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", root.display()))
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    entries.sort();

    assert_eq!(
        entries.len(),
        EXPECTED_NODE_COUNT,
        "real corpus under {} does not have the expected node count — update \
         EXPECTED_NODE_COUNT if the corpus grew on purpose, don't just widen this test",
        root.display()
    );

    let mut failed: Vec<(String, String)> = Vec::new();
    for dir in &entries {
        let id = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        if let Err(e) = abproof::corpus::load_node(dir) {
            failed.push((id, e.to_string()));
        }
    }

    assert!(
        failed.is_empty(),
        "{} of {} real corpus nodes failed to load:\n{}",
        failed.len(),
        entries.len(),
        failed
            .iter()
            .map(|(id, e)| format!("  {id}: {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}
