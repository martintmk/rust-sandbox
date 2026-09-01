// Licensed under the MIT License.

use serde::{Deserialize, Serialize};

use super::AnalysisError;

const MAX_RESULT_BYTES: usize = 64 * 1024;
const MAX_FINDINGS: usize = 100;
const MAX_TEXT_CHARS: usize = 16_384;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AnalysisOutput {
    pub overview: Overview,
    pub interesting: Interest,
    pub review: Review,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Overview {
    pub summary: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Interest {
    pub interesting: bool,
    pub priority: Priority,
    pub rationale: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub verdict: Verdict,
    pub summary: String,
    pub findings: Vec<Finding>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Approve,
    Comment,
    RequestChanges,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub severity: Priority,
    pub title: String,
    pub details: String,
    pub file: Option<String>,
    pub line: Option<u64>,
}

pub(crate) fn parse_overview(raw: &str) -> Result<Overview, AnalysisError> {
    parse_and_validate(raw, |result: &Overview| validate_text("overview summary", &result.summary))
}

pub(crate) fn parse_interesting(raw: &str) -> Result<Interest, AnalysisError> {
    parse_and_validate(raw, |result: &Interest| validate_text("interest rationale", &result.rationale))
}

pub(crate) fn parse_review(raw: &str) -> Result<Review, AnalysisError> {
    parse_and_validate(raw, |result: &Review| {
        validate_text("review summary", &result.summary)?;
        if result.findings.len() > MAX_FINDINGS {
            return Err(AnalysisError::invalid_output(format!(
                "review contains more than {MAX_FINDINGS} findings"
            )));
        }
        for finding in &result.findings {
            validate_text("finding title", &finding.title)?;
            validate_text("finding details", &finding.details)?;
            if finding.file.as_deref().is_some_and(str::is_empty) {
                return Err(AnalysisError::invalid_output("finding file must not be empty"));
            }
            if finding.line == Some(0) {
                return Err(AnalysisError::invalid_output("finding line must be positive"));
            }
        }
        Ok(())
    })
}

fn parse_and_validate<T>(raw: &str, validate: impl FnOnce(&T) -> Result<(), AnalysisError>) -> Result<T, AnalysisError>
where
    T: for<'de> Deserialize<'de>,
{
    if raw.len() > MAX_RESULT_BYTES {
        return Err(AnalysisError::invalid_output(format!("response exceeds {MAX_RESULT_BYTES} bytes")));
    }
    let json = strip_json_fence(raw)?;
    let result = serde_json::from_str(json).map_err(|error| AnalysisError::invalid_output(error.to_string()))?;
    validate(&result)?;
    Ok(result)
}

fn strip_json_fence(raw: &str) -> Result<&str, AnalysisError> {
    let trimmed = raw.trim();
    if !trimmed.starts_with("```") {
        return Ok(trimmed);
    }
    let after_open = trimmed
        .strip_prefix("```json")
        .or_else(|| trimmed.strip_prefix("```JSON"))
        .or_else(|| trimmed.strip_prefix("```"))
        .ok_or_else(|| AnalysisError::invalid_output("invalid JSON fence"))?;
    after_open
        .strip_suffix("```")
        .map(str::trim)
        .ok_or_else(|| AnalysisError::invalid_output("unterminated JSON fence"))
}

fn validate_text(label: &str, text: &str) -> Result<(), AnalysisError> {
    let count = text.chars().count();
    if text.trim().is_empty() {
        return Err(AnalysisError::invalid_output(format!("{label} must not be empty")));
    }
    if count > MAX_TEXT_CHARS {
        return Err(AnalysisError::invalid_output(format!(
            "{label} exceeds {MAX_TEXT_CHARS} characters"
        )));
    }
    Ok(())
}

#[cfg(any(test, feature = "test-util"))]
impl AnalysisOutput {
    pub(crate) fn example() -> Self {
        Self {
            overview: Overview {
                summary: "Updates parser behavior.".to_owned(),
            },
            interesting: Interest {
                interesting: true,
                priority: Priority::High,
                rationale: "Changes public parsing behavior.".to_owned(),
            },
            review: Review {
                verdict: Verdict::Comment,
                summary: "One behavior needs clarification.".to_owned(),
                findings: vec![Finding {
                    severity: Priority::Medium,
                    title: "Ambiguous fallback".to_owned(),
                    details: "The fallback accepts malformed input.".to_owned(),
                    file: Some("src/parser.rs".to_owned()),
                    line: Some(42),
                }],
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_json_fences_but_rejects_unknown_fields() {
        let result = parse_overview("```json\n{\"summary\":\"Short summary\"}\n```").expect("fenced JSON should parse");
        assert_eq!(result.summary, "Short summary");
        assert!(matches!(
            parse_overview(r#"{"summary":"ok","extra":true}"#),
            Err(error) if error.kind() == crate::AnalysisErrorKind::InvalidOutput
        ));
    }

    #[test]
    fn rejects_invalid_review_locations_and_empty_text() {
        assert!(matches!(
            parse_review(
                r#"{"verdict":"comment","summary":"ok","findings":[{"severity":"high","title":"x","details":"y","file":"a.rs","line":0}]}"#,
            ),
            Err(error) if error.kind() == crate::AnalysisErrorKind::InvalidOutput
        ));
        assert!(matches!(
            parse_interesting(r#"{"interesting":false,"priority":"low","rationale":" "}"#),
            Err(error) if error.kind() == crate::AnalysisErrorKind::InvalidOutput
        ));
    }
}
