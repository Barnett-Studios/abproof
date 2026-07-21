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
            provision_ms: 0,
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
