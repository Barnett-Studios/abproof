//! dotclaude-measure — offline A/B change-validation harness (v1, same-loop-local).
//! One-way dependency on dotclaude-core; the engine never sees this crate.
pub mod corpus;
pub mod driver;
pub mod env_filter;
pub mod experiment;
pub mod judge;
pub mod report;
pub mod run;
pub mod score;
pub mod stats;
pub mod worktree;

/// Usage string for the `measure` subcommand (printed on bare/unknown invocation).
pub fn cli_usage() -> &'static str {
    "usage: dotclaude measure run <manifest.yaml> \
     [--dry-run | --confirm] [--out <path>] \
     [--max-cost <usd>] [--max-calls <n>]"
}
