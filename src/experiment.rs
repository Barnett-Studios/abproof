//! Experiment manifest — configuration types parsed from the YAML manifest.

use indexmap::IndexMap;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContextStrategy {
    Cxpak,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Backend {
    #[default]
    Local,
    ClaudeCli,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricTag {
    Gated,
    Tracked,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArmConfig {
    #[serde(rename = "loop")]
    pub loop_name: String,
    pub model: String,
    pub context: ContextStrategy,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub backend: Backend,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub name: String,
    #[serde(default = "default_reps")]
    pub reps: u32,
    #[serde(default = "default_seed_base")]
    pub seed_base: u64,
    pub battery: Vec<String>,
    pub baseline: ArmConfig,
    pub treatment: ArmConfig,
    pub metrics: IndexMap<String, MetricTag>,
    #[serde(default)]
    pub tolerance: BTreeMap<String, f64>,
}

fn default_reps() -> u32 {
    30
}

fn default_seed_base() -> u64 {
    1
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("{0}: {1}")]
    Parse(PathBuf, String),
    #[error("manifest '{0}': {1}")]
    Invalid(String, String),
}

pub fn load_manifest(path: &Path) -> Result<Manifest, ManifestError> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| ManifestError::Parse(path.to_path_buf(), e.to_string()))?;
    serde_yaml::from_str(&content)
        .map_err(|e| ManifestError::Parse(path.to_path_buf(), e.to_string()))
}

impl Manifest {
    pub fn validate(&self) -> Result<(), ManifestError> {
        if self.name.is_empty() {
            return Err(ManifestError::Invalid(
                self.name.clone(),
                "name must not be empty".to_string(),
            ));
        }
        if self.reps < 1 {
            return Err(ManifestError::Invalid(
                self.name.clone(),
                "reps must be >= 1".to_string(),
            ));
        }
        if self.battery.is_empty() {
            return Err(ManifestError::Invalid(
                self.name.clone(),
                "battery must not be empty".to_string(),
            ));
        }
        let has_gated = self.metrics.values().any(|t| *t == MetricTag::Gated);
        if !has_gated {
            return Err(ManifestError::Invalid(
                self.name.clone(),
                "at least one metric must be tagged gated".to_string(),
            ));
        }
        for key in self.tolerance.keys() {
            if !self.metrics.contains_key(key.as_str()) {
                return Err(ManifestError::Invalid(
                    self.name.clone(),
                    format!("tolerance key '{key}' is not a known metric"),
                ));
            }
        }
        for arm in [&self.baseline, &self.treatment] {
            if arm.backend == Backend::ClaudeCli && arm.model.trim().is_empty() {
                return Err(ManifestError::Invalid(
                    self.name.clone(),
                    format!(
                        "arm '{}': claude-cli backend requires a non-empty model",
                        arm.loop_name
                    ),
                ));
            }
        }
        Ok(())
    }

    pub fn gated_metrics(&self) -> Vec<&str> {
        self.metrics
            .iter()
            .filter(|(_, tag)| **tag == MetricTag::Gated)
            .map(|(k, _)| k.as_str())
            .collect()
    }

    pub fn tracked_metrics(&self) -> Vec<&str> {
        self.metrics
            .iter()
            .filter(|(_, tag)| **tag == MetricTag::Tracked)
            .map(|(k, _)| k.as_str())
            .collect()
    }

    pub fn is_same_loop(&self) -> bool {
        self.baseline.loop_name == self.treatment.loop_name
    }

    /// True when one arm is `Local` and the other is `ClaudeCli`.
    /// A cross-loop experiment must satisfy this to be a valid backend-only comparison.
    pub fn is_cross_loop(&self) -> bool {
        (self.baseline.backend == Backend::Local && self.treatment.backend == Backend::ClaudeCli)
            || (self.baseline.backend == Backend::ClaudeCli
                && self.treatment.backend == Backend::Local)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/experiments")
            .join(name)
    }

    #[test]
    fn parses_valid_same_loop() {
        let m = load_manifest(&fixture("valid-same-loop.yaml")).expect("parse");
        m.validate().expect("valid");
        assert_eq!(m.reps, 30);
        assert_eq!(m.gated_metrics(), vec!["node_pass_rate"]);
        assert!(m.tracked_metrics().contains(&"judge_quality"));
        assert!(m.is_same_loop());
        assert_eq!(m.baseline.context, ContextStrategy::None);
        assert_eq!(m.treatment.context, ContextStrategy::Cxpak);
    }

    #[test]
    fn rejects_no_gated_metric() {
        let m = load_manifest(&fixture("no-gated-metric.yaml")).unwrap();
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_, _))));
    }

    #[test]
    fn rejects_tolerance_unknown_key() {
        let m = load_manifest(&fixture("tolerance-unknown-key.yaml")).unwrap();
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_, _))));
    }

    #[test]
    fn rejects_bad_context_at_parse() {
        assert!(matches!(
            load_manifest(&fixture("bad-context.yaml")),
            Err(ManifestError::Parse(_, _))
        ));
    }

    #[test]
    fn rejects_zero_reps() {
        let m = load_manifest(&fixture("zero-reps.yaml")).unwrap();
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_, _))));
    }

    #[test]
    fn rejects_empty_battery() {
        let m = load_manifest(&fixture("empty-battery.yaml")).unwrap();
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_, _))));
    }

    #[test]
    fn backend_defaults_to_local() {
        let m = load_manifest(&fixture("valid-same-loop.yaml")).unwrap();
        assert_eq!(m.baseline.backend, Backend::Local);
        assert_eq!(m.treatment.backend, Backend::Local);
        assert!(!m.is_cross_loop());
    }

    #[test]
    fn parses_cross_loop_manifest() {
        let m = load_manifest(&fixture("cross-loop-valid.yaml")).expect("parse");
        m.validate().expect("valid");
        assert_eq!(m.baseline.backend, Backend::Local);
        assert_eq!(m.treatment.backend, Backend::ClaudeCli);
        assert!(m.is_cross_loop());
        assert!(
            !m.treatment.model.is_empty(),
            "model must be non-empty for claude-cli arm"
        );
    }

    #[test]
    fn rejects_claude_cli_arm_without_model() {
        let m = load_manifest(&fixture("cross-loop-no-model.yaml")).unwrap();
        assert!(matches!(m.validate(), Err(ManifestError::Invalid(_, _))));
    }

    #[test]
    fn backend_deser_kebab_case() {
        let yaml = r#"
name: t
reps: 1
battery: [py-add]
baseline:
  loop: execute-node
  model: local
  context: none
  backend: local
treatment:
  loop: execute-node
  model: claude-haiku-4-5
  context: none
  backend: claude-cli
metrics:
  node_pass_rate: gated
"#;
        let m: Manifest = serde_yaml::from_str(yaml).expect("parse inline yaml");
        m.validate().expect("valid");
        assert_eq!(m.treatment.backend, Backend::ClaudeCli);
        assert_eq!(m.baseline.backend, Backend::Local);
    }
}
