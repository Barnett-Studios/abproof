//! Judge — per-turn verdict produced by the comparison function.

use crate::driver::RunOutput;
use indexmap::IndexMap;

#[derive(Debug, Clone)]
pub struct Rubric {
    pub criteria: Vec<String>,
    pub max_per_criterion: u8,
}

#[derive(Debug, Clone)]
pub struct JudgeScore {
    pub per_criterion: IndexMap<String, u8>,
    pub total: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error("{0}")]
    Dispatch(String),
}

pub trait Judge {
    fn score(&self, output: &RunOutput, rubric: &Rubric) -> Result<JudgeScore, JudgeError>;
}

/// The absence of a judge, made explicit (#8).
///
/// Every call fails, so nothing enters the aggregate and the metric is reported **ABSENT**
/// rather than as a number. This is deliberately not a [`StubJudge`] scoring 0: a canned
/// `0.0` is indistinguishable in the report from a real judge that rated the work as bad,
/// which is the strongest possible false claim about output quality — and it was what the
/// production `run` path shipped.
///
/// Wire a real judge here when one exists. Until [attestr#9] establishes judge ↔ human-label
/// agreement, any such judge is an *uncalibrated instrument* and must be reported as one
/// rather than promoted to ground truth.
///
/// [attestr#9]: https://github.com/Barnett-Studios/attestr/issues/9
pub struct AbsentJudge;

impl Judge for AbsentJudge {
    fn score(&self, _output: &RunOutput, _rubric: &Rubric) -> Result<JudgeScore, JudgeError> {
        Err(JudgeError::Dispatch(
            "no judge configured — judge_quality is ABSENT, not 0.0".to_string(),
        ))
    }
}

pub struct StubJudge {
    pub canned: JudgeScore,
}

impl Judge for StubJudge {
    fn score(&self, _output: &RunOutput, _rubric: &Rubric) -> Result<JudgeScore, JudgeError> {
        Ok(self.canned.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{RunOutput, RunStatus};

    fn any_output() -> RunOutput {
        RunOutput {
            status: RunStatus::Success,
            accept_passed: true,
            edited_files: vec![],
            stdout_tail: "SUCCESS".into(),
            duration_ms: 1,
            cost_usd: Some(0.0),
            input_tokens: 0,
            output_tokens: 0,
            claude_calls: 0,
            num_turns: 0,
            seeds_honoured: false,
        }
    }

    #[test]
    fn absent_judge_always_fails_and_never_scores() {
        // The point of #8: the absence of a judge must be a dispatch failure, so no value
        // reaches the aggregate. A judge that "succeeds" with 0 is indistinguishable in
        // the report from a real judge rating the work as bad.
        let err = AbsentJudge
            .score(
                &any_output(),
                &Rubric {
                    criteria: vec!["clarity".into()],
                    max_per_criterion: 4,
                },
            )
            .expect_err("AbsentJudge must never return a score");
        assert!(
            err.to_string().contains("no judge configured"),
            "the error must name the cause; got {err}"
        );
    }

    #[test]
    fn stub_judge_returns_canned() {
        let mut per_criterion = IndexMap::new();
        per_criterion.insert("clarity".to_string(), 3u8);
        let s = JudgeScore {
            per_criterion,
            total: 3,
        };
        let j = StubJudge { canned: s };
        let out = RunOutput {
            status: RunStatus::Success,
            accept_passed: true,
            edited_files: vec![],
            stdout_tail: "SUCCESS".into(),
            duration_ms: 1,
            cost_usd: Some(0.0),
            input_tokens: 0,
            output_tokens: 0,
            claude_calls: 0,
            num_turns: 0,
            seeds_honoured: false,
        };
        let result = j
            .score(
                &out,
                &Rubric {
                    criteria: vec!["clarity".into()],
                    max_per_criterion: 4,
                },
            )
            .unwrap();
        assert_eq!(result.total, 3);
    }
}
