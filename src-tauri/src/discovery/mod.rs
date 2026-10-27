//! Validation boundary for editorial suggestions returned by an AI provider.
//!
//! Provider adapters should request JSON with a `suggestions` array, pass only
//! bounded transcript windows, and validate proposals before persistence.
//! A validated suggestion is still only a proposal for the user to review.

use crate::transcription::TranscriptSegment;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

mod adapters;
pub mod jobs;

pub use adapters::{AdapterError, Provider, discover_moments};

const MAX_SUGGESTIONS: usize = 20;
const MAX_TEXT_BYTES: usize = 2_000;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const TIME_TOLERANCE_SECONDS: f64 = 0.001;
const QUOTE_BOUNDARY_TOLERANCE_SECONDS: f64 = 1.0;
const MAX_QUOTE_SEGMENT_GAP_SECONDS: f64 = 1.0;
pub(super) const MIN_GENERATED_CLIP_SECONDS: f64 = 120.0;
pub(super) const MAX_GENERATED_CLIP_SECONDS: f64 = 300.0;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TimeRange {
    pub start_seconds: f64,
    pub end_seconds: f64,
}

impl TimeRange {
    fn is_valid_within(self, duration_seconds: f64) -> bool {
        self.start_seconds.is_finite()
            && self.end_seconds.is_finite()
            && self.start_seconds >= 0.0
            && self.end_seconds > self.start_seconds
            && self.end_seconds <= duration_seconds + TIME_TOLERANCE_SECONDS
    }

    fn contains(self, other: Self) -> bool {
        other.start_seconds + TIME_TOLERANCE_SECONDS >= self.start_seconds
            && other.end_seconds <= self.end_seconds + TIME_TOLERANCE_SECONDS
    }
}

fn segment_range(segment: &TranscriptSegment) -> TimeRange {
    TimeRange {
        start_seconds: segment.start_seconds,
        end_seconds: segment.end_seconds,
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawSuggestion {
    start_seconds: f64,
    end_seconds: f64,
    hook: String,
    rationale: String,
    source_quote: String,
    score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatedSuggestion {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub hook: String,
    pub rationale: String,
    pub source_quote: String,
    /// Optional editorial ranking hint supplied by the model, between 0 and 1.
    pub score: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ParsedSuggestions {
    pub suggestions: Vec<ValidatedSuggestion>,
    pub rejected_count: usize,
    pub first_rejection: Option<DiscoveryError>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiscoveryError {
    InvalidTranscript,
    InvalidAnalysisWindow,
    InvalidResponse(&'static str),
    InvalidResponseField(usize, &'static str),
    InvalidSuggestion(usize, &'static str),
    AllSuggestionsRejected(usize, usize, &'static str),
}

impl std::fmt::Display for DiscoveryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTranscript => {
                write!(formatter, "The transcript contains invalid timing or text.")
            }
            Self::InvalidAnalysisWindow => write!(formatter, "The analysis window is invalid."),
            Self::InvalidResponse(reason) => write!(
                formatter,
                "The provider returned an invalid suggestion response: {reason}"
            ),
            Self::InvalidResponseField(index, reason) => write!(
                formatter,
                "The provider returned an invalid suggestion response at suggestion {}: {reason}",
                index + 1
            ),
            Self::InvalidSuggestion(index, reason) => {
                write!(formatter, "Suggestion {} was rejected: {reason}", index + 1)
            }
            Self::AllSuggestionsRejected(count, index, reason) => write!(
                formatter,
                "All {count} suggestions were rejected. Suggestion {}: {reason}",
                index + 1
            ),
        }
    }
}

/// Parse a provider's JSON response and verify every proposed cut and quote.
///
/// `analysis_window` is the exact source-time window submitted to the provider.
/// Quote comparison ignores case, accents, and punctuation, but requires the same
/// words in the same order in transcript segments overlapping the suggested cut.
pub fn parse_suggestions(
    json: &str,
    transcript: &[TranscriptSegment],
    media_duration_seconds: f64,
    analysis_window: TimeRange,
) -> Result<Vec<ValidatedSuggestion>, DiscoveryError> {
    parse_suggestions_detailed(json, transcript, media_duration_seconds, analysis_window)
        .map(|result| result.suggestions)
}

/// Retain valid proposals while reporting individually rejected model output.
pub fn parse_suggestions_detailed(
    json: &str,
    transcript: &[TranscriptSegment],
    media_duration_seconds: f64,
    analysis_window: TimeRange,
) -> Result<ParsedSuggestions, DiscoveryError> {
    parse_suggestions_with_minimum_duration(
        json,
        transcript,
        media_duration_seconds,
        analysis_window,
        MIN_GENERATED_CLIP_SECONDS,
    )
}

fn parse_suggestions_with_minimum_duration(
    json: &str,
    transcript: &[TranscriptSegment],
    media_duration_seconds: f64,
    analysis_window: TimeRange,
    minimum_duration_seconds: f64,
) -> Result<ParsedSuggestions, DiscoveryError> {
    if json.len() > MAX_RESPONSE_BYTES {
        return Err(DiscoveryError::InvalidResponse(
            "the JSON exceeds the 256 KiB limit",
        ));
    }
    if !media_duration_seconds.is_finite() || media_duration_seconds <= 0.0 {
        return Err(DiscoveryError::InvalidTranscript);
    }
    if !analysis_window.is_valid_within(media_duration_seconds) {
        return Err(DiscoveryError::InvalidAnalysisWindow);
    }
    let mut previous_start = 0.0;
    for segment in transcript {
        if !segment_range(segment).is_valid_within(media_duration_seconds)
            || segment.start_seconds + TIME_TOLERANCE_SECONDS < previous_start
            || segment.text.trim().is_empty()
            || segment.text.len() > MAX_TEXT_BYTES
        {
            return Err(DiscoveryError::InvalidTranscript);
        }
        previous_start = segment.start_seconds;
    }

    let value: Value = serde_json::from_str(json)
        .map_err(|_| DiscoveryError::InvalidResponse("JSON syntax is invalid"))?;
    let object = value
        .as_object()
        .ok_or(DiscoveryError::InvalidResponse("expected a JSON object"))?;
    if object.len() != 1 || !object.contains_key("suggestions") {
        return Err(DiscoveryError::InvalidResponse(
            "unexpected fields or unsupported JSON structure",
        ));
    }
    let suggestions = object.get("suggestions").and_then(Value::as_array).ok_or(
        DiscoveryError::InvalidResponse("expected a suggestions array"),
    )?;
    if suggestions.len() > MAX_SUGGESTIONS {
        return Err(DiscoveryError::InvalidResponse(
            "more than 20 suggestions were returned",
        ));
    }
    let mut validated = Vec::with_capacity(suggestions.len());
    let mut rejected_count = 0;
    let mut first_rejection = None;
    for (index, suggestion) in suggestions.iter().enumerate() {
        match validate_suggestion(
            suggestion,
            index,
            transcript,
            media_duration_seconds,
            analysis_window,
            minimum_duration_seconds,
        ) {
            Ok(suggestion) => validated.push(suggestion),
            Err(error) => {
                rejected_count += 1;
                if first_rejection.is_none() {
                    first_rejection = Some(error);
                }
            }
        }
    }
    if !suggestions.is_empty() && validated.is_empty() {
        let first = first_rejection.expect("a rejected suggestion has a reason");
        if rejected_count == 1 {
            return Err(first);
        }
        let (index, reason) = match first {
            DiscoveryError::InvalidResponseField(index, reason)
            | DiscoveryError::InvalidSuggestion(index, reason) => (index, reason),
            _ => unreachable!("individual validation returns an indexed error"),
        };
        return Err(DiscoveryError::AllSuggestionsRejected(
            rejected_count,
            index,
            reason,
        ));
    }

    // Rank verified cuts by the provider's editorial score before collapsing
    // near duplicates. Stable sorting preserves provider order on tied scores.
    validated.sort_by(|left, right| {
        right
            .score
            .unwrap_or(-1.0)
            .total_cmp(&left.score.unwrap_or(-1.0))
    });
    // Keep distinct editorial arcs even when their footage overlaps.
    let mut unique: Vec<ValidatedSuggestion> = Vec::with_capacity(validated.len());
    for suggestion in validated {
        let duplicates_existing = unique.iter().any(|existing| {
            let overlap = (suggestion.end_seconds.min(existing.end_seconds)
                - suggestion.start_seconds.max(existing.start_seconds))
            .max(0.0);
            let union = suggestion.end_seconds.max(existing.end_seconds)
                - suggestion.start_seconds.min(existing.start_seconds);
            union > 0.0 && overlap / union >= 0.85
        });
        if !duplicates_existing {
            unique.push(suggestion);
        }
    }
    Ok(ParsedSuggestions {
        suggestions: unique,
        rejected_count,
        first_rejection,
    })
}

fn validate_suggestion(
    value: &Value,
    index: usize,
    transcript: &[TranscriptSegment],
    media_duration_seconds: f64,
    analysis_window: TimeRange,
    minimum_duration_seconds: f64,
) -> Result<ValidatedSuggestion, DiscoveryError> {
    {
        let Some(fields) = value.as_object() else {
            return Err(DiscoveryError::InvalidResponseField(
                index,
                "expected an object with cut timing, text, and score fields",
            ));
        };
        for field in ["startSeconds", "endSeconds"] {
            if !fields.get(field).is_some_and(Value::is_number) {
                return Err(DiscoveryError::InvalidResponseField(
                    index,
                    if field == "startSeconds" {
                        "startSeconds must be a number"
                    } else {
                        "endSeconds must be a number"
                    },
                ));
            }
        }
        for field in ["hook", "rationale", "sourceQuote"] {
            if !fields.get(field).is_some_and(Value::is_string) {
                return Err(DiscoveryError::InvalidResponseField(
                    index,
                    match field {
                        "hook" => "hook must be a string",
                        "rationale" => "rationale must be a string",
                        _ => "sourceQuote must be a string",
                    },
                ));
            }
        }
        if fields
            .get("score")
            .is_some_and(|value| !value.is_null() && !value.is_number())
        {
            return Err(DiscoveryError::InvalidResponseField(
                index,
                "score must be a number or null",
            ));
        }
    }
    let suggestion: RawSuggestion = serde_json::from_value(value.clone()).map_err(|_| {
        DiscoveryError::InvalidResponseField(
            index,
            "unexpected fields or unsupported JSON structure",
        )
    })?;
    let range = TimeRange {
        start_seconds: suggestion.start_seconds,
        end_seconds: suggestion.end_seconds,
    };
    let valid_text = [
        &suggestion.hook,
        &suggestion.rationale,
        &suggestion.source_quote,
    ]
    .iter()
    .all(|value| !value.trim().is_empty() && value.len() <= MAX_TEXT_BYTES);
    let valid_score = suggestion
        .score
        .is_none_or(|score| score.is_finite() && (0.0..=1.0).contains(&score));
    if !range.is_valid_within(media_duration_seconds) {
        return Err(DiscoveryError::InvalidSuggestion(
            index,
            "startSeconds/endSeconds are invalid or outside the recording",
        ));
    }
    if !analysis_window.contains(range) {
        return Err(DiscoveryError::InvalidSuggestion(
            index,
            "the proposed cut is outside the analyzed transcript window",
        ));
    }
    let cut_duration = range.end_seconds - range.start_seconds;
    if cut_duration + TIME_TOLERANCE_SECONDS < minimum_duration_seconds
        || cut_duration > MAX_GENERATED_CLIP_SECONDS + TIME_TOLERANCE_SECONDS
    {
        return Err(DiscoveryError::InvalidSuggestion(
            index,
            "the proposed cut must be between 120 and 300 seconds",
        ));
    }
    if !valid_text {
        return Err(DiscoveryError::InvalidSuggestion(
            index,
            "hook, rationale, or sourceQuote is empty or exceeds 2,000 bytes",
        ));
    }
    if !valid_score {
        return Err(DiscoveryError::InvalidSuggestion(
            index,
            "score must be a finite number from 0 to 1 or null",
        ));
    }
    if !quote_is_in_range(&suggestion.source_quote, transcript, range) {
        let reason = if quote_is_in_range(&suggestion.source_quote, transcript, analysis_window) {
            "sourceQuote is in the transcript but outside the proposed cut"
        } else {
            "sourceQuote words do not occur in the analyzed transcript in order"
        };
        return Err(DiscoveryError::InvalidSuggestion(index, reason));
    }
    Ok(ValidatedSuggestion {
        start_seconds: range.start_seconds,
        end_seconds: range.end_seconds,
        hook: suggestion.hook,
        rationale: suggestion.rationale,
        source_quote: suggestion.source_quote,
        score: suggestion.score,
    })
}

fn quote_is_in_range(quote: &str, transcript: &[TranscriptSegment], range: TimeRange) -> bool {
    let quote_words = words(quote);
    if quote_words.is_empty() {
        return false;
    }
    // Phrase segments do not carry individual word timestamps. Estimate each
    // word's position within its phrase so a cut through a phrase can still
    // match its quote without allowing a barely touching cut to claim it.
    let source_words: Vec<(String, TimeRange, usize)> = transcript
        .iter()
        .enumerate()
        .flat_map(|(segment_index, segment)| {
            let segment_words = words(&segment.text);
            let count = segment_words.len();
            segment_words
                .into_iter()
                .enumerate()
                .map(move |(index, word)| {
                    let width = (segment.end_seconds - segment.start_seconds) / count as f64;
                    (
                        word,
                        TimeRange {
                            start_seconds: segment.start_seconds + index as f64 * width,
                            end_seconds: segment.start_seconds + (index + 1) as f64 * width,
                        },
                        segment_index,
                    )
                })
        })
        .collect();
    source_words.windows(quote_words.len()).any(|window| {
        if !window
            .iter()
            .map(|(word, _, _)| word)
            .eq(quote_words.iter())
        {
            return false;
        }
        if window.windows(2).any(|pair| {
            let previous = pair[0].2;
            let next = pair[1].2;
            previous != next
                && (next != previous + 1
                    || transcript[next].start_seconds - transcript[previous].end_seconds
                        > MAX_QUOTE_SEGMENT_GAP_SECONDS
                    || matches!(
                        (&transcript[previous].speaker, &transcript[next].speaker),
                        (Some(left), Some(right)) if left != right
                    ))
        }) {
            return false;
        }
        let quote_start = window.first().unwrap().1.start_seconds;
        let quote_end = window.last().unwrap().1.end_seconds;
        let overlap =
            (quote_end.min(range.end_seconds) - quote_start.max(range.start_seconds)).max(0.0);
        quote_start >= range.start_seconds - QUOTE_BOUNDARY_TOLERANCE_SECONDS
            && quote_end <= range.end_seconds + QUOTE_BOUNDARY_TOLERANCE_SECONDS
            && overlap + TIME_TOLERANCE_SECONDS >= (quote_end - quote_start) * 0.5
    })
}

fn words(text: &str) -> Vec<String> {
    text.nfkd()
        .filter(|character| !is_combining_mark(*character))
        .collect::<String>()
        .to_lowercase()
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Isolate quote and response-shape tests from the product's generated-cut
    // duration policy. These tiny fixtures target only source alignment.
    fn parse_quote_suggestions(
        json: &str,
        transcript: &[TranscriptSegment],
        media_duration_seconds: f64,
        analysis_window: TimeRange,
    ) -> Result<Vec<ValidatedSuggestion>, DiscoveryError> {
        parse_quote_suggestions_detailed(json, transcript, media_duration_seconds, analysis_window)
            .map(|result| result.suggestions)
    }

    fn parse_quote_suggestions_detailed(
        json: &str,
        transcript: &[TranscriptSegment],
        media_duration_seconds: f64,
        analysis_window: TimeRange,
    ) -> Result<ParsedSuggestions, DiscoveryError> {
        parse_suggestions_with_minimum_duration(
            json,
            transcript,
            media_duration_seconds,
            analysis_window,
            0.0,
        )
    }

    fn transcript() -> Vec<TranscriptSegment> {
        vec![
            TranscriptSegment {
                start_seconds: 10.0,
                end_seconds: 12.0,
                text: "A primeira ideia".into(),
                speaker: Some("A".into()),
            },
            TranscriptSegment {
                start_seconds: 12.0,
                end_seconds: 14.0,
                text: "mudou tudo para o grupo.".into(),
                speaker: Some("A".into()),
            },
            TranscriptSegment {
                start_seconds: 20.0,
                end_seconds: 22.0,
                text: "Outro assunto depois.".into(),
                speaker: Some("B".into()),
            },
        ]
    }

    fn json(start: f64, end: f64, quote: &str) -> String {
        serde_json::json!({"suggestions": [{"startSeconds": start, "endSeconds": end, "hook": "A change", "rationale": "Clear moment", "sourceQuote": quote, "score": 0.8}]}).to_string()
    }

    #[test]
    fn accepts_quote_across_adjacent_segments_with_case_and_punctuation_changes() {
        let result = parse_quote_suggestions(
            &json(10.0, 14.0, "IDEIA, mudou tudo"),
            &transcript(),
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        )
        .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].score, Some(0.8));
    }

    #[test]
    fn rejects_quote_spliced_across_a_pause_or_speaker_change() {
        let mut segments = transcript();
        segments[1].start_seconds = 18.0;
        segments[1].end_seconds = 20.0;
        let window = TimeRange {
            start_seconds: 10.0,
            end_seconds: 22.0,
        };
        assert_eq!(
            parse_quote_suggestions(&json(10.0, 20.0, "ideia mudou"), &segments, 60.0, window),
            Err(DiscoveryError::InvalidSuggestion(
                0,
                "sourceQuote words do not occur in the analyzed transcript in order"
            ))
        );
        let mut segments = transcript();
        segments[1].speaker = Some("B".into());
        assert_eq!(
            parse_quote_suggestions(&json(10.0, 14.0, "ideia mudou"), &segments, 60.0, window),
            Err(DiscoveryError::InvalidSuggestion(
                0,
                "sourceQuote words do not occur in the analyzed transcript in order"
            ))
        );
    }

    #[test]
    fn accepts_exact_words_at_a_phrase_boundary_and_normalizes_accents() {
        let result = parse_quote_suggestions(
            &json(10.6, 13.4, "PRIMEIRA ideia múdou, tudo"),
            &transcript(),
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        )
        .unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn rejects_quote_when_cut_only_touches_the_phrase_tail() {
        let result = parse_quote_suggestions(
            &json(11.95, 13.0, "A primeira ideia"),
            &transcript(),
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        );
        assert_eq!(
            result,
            Err(DiscoveryError::InvalidSuggestion(
                0,
                "sourceQuote is in the transcript but outside the proposed cut"
            ))
        );
    }

    #[test]
    fn rejects_quote_with_one_invented_word_after_normalization() {
        let result = parse_quote_suggestions(
            &json(10.0, 14.0, "A primeira ideia inventada"),
            &transcript(),
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        );
        assert_eq!(
            result,
            Err(DiscoveryError::InvalidSuggestion(
                0,
                "sourceQuote words do not occur in the analyzed transcript in order"
            ))
        );
    }

    #[test]
    fn preserves_sourced_suggestions_when_others_have_invented_quotes_or_bad_fields() {
        let response = serde_json::json!({"suggestions": [
            {"startSeconds": 10.0, "endSeconds": 14.0, "hook": "Wrong", "rationale": "Strong", "sourceQuote": "words that were never said"},
            {"startSeconds": 10.0, "endSeconds": 14.0, "hook": "Real", "rationale": "Strong", "sourceQuote": "A primeira ideia"},
            {"startSeconds": 20.0, "endSeconds": 22.0, "hook": "Malformed", "rationale": "Strong", "score": 0.7},
            {"startSeconds": 20.0, "endSeconds": 22.0, "hook": "Also real", "rationale": "Strong", "sourceQuote": "Outro assunto"}
        ]}).to_string();
        let outcome = parse_quote_suggestions_detailed(
            &response,
            &transcript(),
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        )
        .unwrap();
        assert_eq!(outcome.suggestions.len(), 2);
        assert_eq!(outcome.suggestions[0].hook, "Real");
        assert_eq!(outcome.suggestions[1].hook, "Also real");
        assert_eq!(outcome.rejected_count, 2);
        assert_eq!(
            outcome.first_rejection,
            Some(DiscoveryError::InvalidSuggestion(
                0,
                "sourceQuote words do not occur in the analyzed transcript in order"
            ))
        );
    }

    #[test]
    fn reports_count_when_every_proposal_is_unsourced() {
        let response = serde_json::json!({"suggestions": [
            {"startSeconds": 10.0, "endSeconds": 14.0, "hook": "Wrong", "rationale": "Strong", "sourceQuote": "invented quote one"},
            {"startSeconds": 20.0, "endSeconds": 22.0, "hook": "Wrong", "rationale": "Strong", "sourceQuote": "invented quote two"}
        ]}).to_string();
        assert_eq!(
            parse_quote_suggestions_detailed(
                &response,
                &transcript(),
                60.0,
                TimeRange {
                    start_seconds: 10.0,
                    end_seconds: 22.0
                }
            ),
            Err(DiscoveryError::AllSuggestionsRejected(
                2,
                0,
                "sourceQuote words do not occur in the analyzed transcript in order"
            ))
        );
    }

    #[test]
    fn empty_suggestion_array_is_a_valid_empty_result() {
        let outcome = parse_quote_suggestions_detailed(
            r#"{"suggestions":[]}"#,
            &transcript(),
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        )
        .unwrap();
        assert!(outcome.suggestions.is_empty());
        assert_eq!(outcome.rejected_count, 0);
        assert_eq!(outcome.first_rejection, None);
    }

    #[test]
    fn rejects_invented_quote_and_out_of_window_cut() {
        let window = TimeRange {
            start_seconds: 10.0,
            end_seconds: 14.0,
        };
        assert_eq!(
            parse_quote_suggestions(
                &json(10.0, 14.0, "algo inventado"),
                &transcript(),
                60.0,
                window
            ),
            Err(DiscoveryError::InvalidSuggestion(
                0,
                "sourceQuote words do not occur in the analyzed transcript in order"
            ))
        );
        assert_eq!(
            parse_quote_suggestions(
                &json(20.0, 22.0, "Outro assunto"),
                &transcript(),
                60.0,
                window
            ),
            Err(DiscoveryError::InvalidSuggestion(
                0,
                "the proposed cut is outside the analyzed transcript window"
            ))
        );
    }

    #[test]
    fn rejects_quote_outside_selected_cut() {
        let result = parse_quote_suggestions(
            &json(12.0, 14.0, "A primeira ideia"),
            &transcript(),
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        );
        assert_eq!(
            result,
            Err(DiscoveryError::InvalidSuggestion(
                0,
                "sourceQuote is in the transcript but outside the proposed cut"
            ))
        );
    }

    #[test]
    fn rejects_malformed_provider_shape_and_unsorted_transcript() {
        let range = TimeRange {
            start_seconds: 10.0,
            end_seconds: 22.0,
        };
        assert_eq!(
            parse_quote_suggestions(
                r#"{"suggestions":[{"startSeconds":"10"}]}"#,
                &transcript(),
                60.0,
                range
            ),
            Err(DiscoveryError::InvalidResponseField(
                0,
                "startSeconds must be a number"
            ))
        );
        let mut unsorted = transcript();
        unsorted.swap(0, 1);
        assert_eq!(
            parse_quote_suggestions(&json(10.0, 14.0, "mudou tudo"), &unsorted, 60.0, range),
            Err(DiscoveryError::InvalidTranscript)
        );
    }

    #[test]
    fn reports_missing_source_quote_field_without_exposing_response_content() {
        let response = serde_json::json!({"suggestions": [
            {"startSeconds": 10.0, "endSeconds": 14.0, "hook": "A change", "rationale": "Clear", "score": 0.8}
        ]})
        .to_string();
        let error = parse_quote_suggestions(
            &response,
            &transcript(),
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        )
        .unwrap_err();
        assert_eq!(
            error,
            DiscoveryError::InvalidResponseField(0, "sourceQuote must be a string")
        );
        assert_eq!(
            error.to_string(),
            "The provider returned an invalid suggestion response at suggestion 1: sourceQuote must be a string"
        );
    }

    #[test]
    fn keeps_first_ranked_suggestion_when_cuts_overlap() {
        let response = serde_json::json!({"suggestions": [
            {"startSeconds": 10.0, "endSeconds": 14.0, "hook": "First", "rationale": "Strong", "sourceQuote": "A primeira ideia"},
            {"startSeconds": 10.0, "endSeconds": 13.8, "hook": "Second", "rationale": "Also strong", "sourceQuote": "A primeira ideia"},
            {"startSeconds": 20.0, "endSeconds": 22.0, "hook": "Third", "rationale": "Separate", "sourceQuote": "Outro assunto"}
        ]}).to_string();
        let suggestions = parse_quote_suggestions(
            &response,
            &transcript(),
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        )
        .unwrap();
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].hook, "First");
        assert_eq!(suggestions[1].hook, "Third");
    }

    #[test]
    fn higher_scored_verified_cut_leads_and_wins_near_duplicate() {
        let response = serde_json::json!({"suggestions": [
            {"startSeconds": 10.0, "endSeconds": 14.0, "hook": "Lower", "rationale": "One arc", "sourceQuote": "A primeira ideia", "score": 0.55},
            {"startSeconds": 10.1, "endSeconds": 14.0, "hook": "Stronger", "rationale": "Better arc", "sourceQuote": "A primeira ideia", "score": 0.91},
            {"startSeconds": 20.0, "endSeconds": 22.0, "hook": "Other", "rationale": "Separate", "sourceQuote": "Outro assunto", "score": 0.73}
        ]}).to_string();
        let suggestions = parse_quote_suggestions(
            &response,
            &transcript(),
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        )
        .unwrap();
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].hook, "Stronger");
        assert_eq!(suggestions[1].hook, "Other");
    }

    #[test]
    fn permits_sorted_words_with_overlapping_speaker_timestamps() {
        let mut segments = transcript();
        segments[0].end_seconds = 12.5;
        let suggestions = parse_quote_suggestions(
            &json(10.0, 14.0, "ideia mudou"),
            &segments,
            60.0,
            TimeRange {
                start_seconds: 10.0,
                end_seconds: 22.0,
            },
        )
        .unwrap();
        assert_eq!(suggestions.len(), 1);
    }

    #[test]
    fn generated_cuts_must_run_two_to_five_minutes_and_keep_a_sourced_quote() {
        let transcript = vec![TranscriptSegment {
            start_seconds: 10.0,
            end_seconds: 11.0,
            text: "This is the central point".into(),
            speaker: None,
        }];
        let window = TimeRange {
            start_seconds: 0.0,
            end_seconds: 600.0,
        };
        for (duration, accepted) in [(119.0, false), (120.0, true), (300.0, true), (301.0, false)] {
            let result = parse_suggestions(
                &json(10.0, 10.0 + duration, "central point"),
                &transcript,
                600.0,
                window,
            );
            assert_eq!(result.is_ok(), accepted, "duration {duration}");
            if !accepted {
                assert_eq!(
                    result,
                    Err(DiscoveryError::InvalidSuggestion(
                        0,
                        "the proposed cut must be between 120 and 300 seconds"
                    ))
                );
            }
        }
        assert_eq!(
            parse_suggestions(
                &json(10.0, 130.0, "invented point"),
                &transcript,
                600.0,
                window
            ),
            Err(DiscoveryError::InvalidSuggestion(
                0,
                "sourceQuote words do not occur in the analyzed transcript in order"
            ))
        );
    }

    #[test]
    fn overlapping_but_distinct_long_cuts_remain_available_for_review() {
        let transcript = vec![TranscriptSegment {
            start_seconds: 10.0,
            end_seconds: 11.0,
            text: "A grounded anchor".into(),
            speaker: None,
        }];
        let response = serde_json::json!({"suggestions": [
            {"startSeconds": 10.0, "endSeconds": 140.0, "hook": "First", "rationale": "Arc one", "sourceQuote": "grounded anchor"},
            {"startSeconds": 10.0, "endSeconds": 141.0, "hook": "Near copy", "rationale": "Same arc", "sourceQuote": "grounded anchor"},
            {"startSeconds": 10.0, "endSeconds": 250.0, "hook": "Extended arc", "rationale": "A different cut", "sourceQuote": "grounded anchor"}
        ]}).to_string();
        let suggestions = parse_suggestions(
            &response,
            &transcript,
            600.0,
            TimeRange {
                start_seconds: 0.0,
                end_seconds: 600.0,
            },
        )
        .unwrap();
        assert_eq!(suggestions.len(), 2);
        assert_eq!(suggestions[0].hook, "First");
        assert_eq!(suggestions[1].hook, "Extended arc");
    }

    #[test]
    fn rejects_oversized_provider_response_before_parsing() {
        let response = serde_json::json!({"suggestions": [{
            "startSeconds": 10.0,
            "endSeconds": 12.0,
            "hook": "x".repeat(MAX_RESPONSE_BYTES),
            "rationale": "Strong",
            "sourceQuote": "A primeira ideia"
        }]})
        .to_string();
        assert!(response.len() > MAX_RESPONSE_BYTES);
        assert_eq!(
            parse_quote_suggestions(
                &response,
                &transcript(),
                60.0,
                TimeRange {
                    start_seconds: 10.0,
                    end_seconds: 22.0
                }
            ),
            Err(DiscoveryError::InvalidResponse(
                "the JSON exceeds the 256 KiB limit"
            ))
        );
    }
}
