//! Source-aligned transcript data and Gemini transcription response validation.
//!
//! Network requests, credentials, audio extraction, and persistence belong to
//! separate backend services. None of those values should cross the webview.

pub mod elevenlabs;
pub mod extraction;
pub mod gemini;
pub mod jobs;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptionProvider {
    Gemini,
    Elevenlabs,
}

impl TranscriptionProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::Elevenlabs => "elevenlabs",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "gemini" => Some(Self::Gemini),
            "elevenlabs" => Some(Self::Elevenlabs),
            _ => None,
        }
    }
}

#[cfg(test)]
mod phrase_tests {
    use super::{TranscriptSegment, group_timed_words};

    fn word(text: &str, start: f64, end: f64, speaker: &str) -> TranscriptSegment {
        TranscriptSegment {
            text: text.into(),
            start_seconds: start,
            end_seconds: end,
            speaker: Some(speaker.into()),
        }
    }

    #[test]
    fn groups_words_but_preserves_sentence_speaker_gap_and_source_times() {
        let phrases = group_timed_words(vec![
            word("world.", 0.4, 0.7, "a"),
            word("Hello", 0.0, 0.3, "a"),
            word("Next", 0.8, 1.1, "a"),
            word("speaker", 1.2, 1.5, "b"),
            word("later", 3.0, 3.2, "b"),
        ]);
        assert_eq!(phrases.len(), 4);
        assert_eq!(phrases[0].text, "Hello world.");
        assert_eq!(
            (phrases[0].start_seconds, phrases[0].end_seconds),
            (0.0, 0.7)
        );
        assert_eq!(phrases[1].text, "Next");
        assert_eq!(phrases[2].speaker.as_deref(), Some("b"));
        assert_eq!(phrases[3].text, "later");
    }

    #[test]
    fn long_speech_becomes_bounded_caption_phrases() {
        let words = (0..30)
            .map(|index| word("spoken", index as f64 * 0.3, index as f64 * 0.3 + 0.2, "a"))
            .collect();
        let phrases = group_timed_words(words);
        assert_eq!(
            phrases
                .iter()
                .map(|phrase| phrase.text.split_whitespace().count())
                .sum::<usize>(),
            30
        );
        assert!(
            phrases
                .iter()
                .all(|phrase| phrase.text.split_whitespace().count() <= 12)
        );
        assert!(
            phrases
                .iter()
                .all(|phrase| phrase.end_seconds - phrase.start_seconds <= 6.0)
        );
    }
}

const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_WORDS_PER_CHUNK: usize = 200_000;
const MAX_CHUNKS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptSegment {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub text: String,
    pub speaker: Option<String>,
}

/// Turn timed provider words into readable, editable phrases while retaining
/// source-aligned ranges for discovery and captions. Provider text stays
/// untrusted; grouping changes only local presentation granularity.
pub fn group_timed_words(mut words: Vec<TranscriptSegment>) -> Vec<TranscriptSegment> {
    const MAX_WORDS: usize = 12;
    const MAX_CHARACTERS: usize = 160;
    const MAX_SECONDS: f64 = 6.0;
    const MAX_GAP_SECONDS: f64 = 0.8;
    words.sort_by(|a, b| a.start_seconds.total_cmp(&b.start_seconds));
    let mut phrases = Vec::new();
    let mut current: Option<TranscriptSegment> = None;
    let mut count = 0usize;
    for word in words {
        if let Some(phrase) = current.as_mut() {
            let prior_ends_sentence = phrase
                .text
                .chars()
                .last()
                .is_some_and(|last| matches!(last, '.' | '?' | '!'));
            let needs_new = phrase.speaker != word.speaker
                || word.start_seconds - phrase.end_seconds > MAX_GAP_SECONDS
                || word.end_seconds - phrase.start_seconds > MAX_SECONDS
                || count >= MAX_WORDS
                || phrase.text.len() + word.text.len() + 1 > MAX_CHARACTERS
                || prior_ends_sentence;
            if needs_new {
                phrases.push(current.take().unwrap());
                current = Some(word);
                count = 1;
            } else {
                phrase.text.push(' ');
                phrase.text.push_str(&word.text);
                phrase.end_seconds = phrase.end_seconds.max(word.end_seconds);
                count += 1;
            }
        } else {
            current = Some(word);
            count = 1;
        }
    }
    if let Some(phrase) = current {
        phrases.push(phrase);
    }
    phrases
}

/// An audio file is extracted locally from the immutable source. Its times are
/// expressed on the source timeline, while Gemini timestamps start at zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioChunk {
    pub index: usize,
    pub start_ms: u64,
    pub end_ms: u64,
    /// The exclusive ownership range used to discard overlap duplicates.
    pub core_start_ms: u64,
    pub core_end_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptionError {
    InvalidChunkPlan,
    ResponseTooLarge,
    InvalidResponse,
    IncompleteResponse,
    MissingWordAnnotations,
    InvalidWord {
        index: usize,
        reason: &'static str,
    },
    ReversedWord {
        index: usize,
        start_offset_ms: u64,
        end_offset_ms: u64,
    },
    WordOutsideChunk {
        index: usize,
        end_offset_ms: u64,
        chunk_duration_ms: u64,
    },
    TimestampOverflow,
}

const MAX_END_OVERHANG_NS: u128 = 50_000_000;
const NANOS_PER_MILLISECOND: u128 = 1_000_000;
const MAX_REVERSED_WORD_NS: u128 = 250_000_000;
const POINT_WORD_DURATION_NS: u128 = 100_000_000;

/// Plan overlapping chunks below the provider's configured duration limit.
/// A midpoint in each overlap assigns every source instant to one chunk.
pub fn plan_chunks(
    duration_ms: u64,
    max_chunk_ms: u64,
    overlap_ms: u64,
) -> Result<Vec<AudioChunk>, TranscriptionError> {
    if duration_ms == 0 || max_chunk_ms == 0 || overlap_ms >= max_chunk_ms {
        return Err(TranscriptionError::InvalidChunkPlan);
    }

    let mut spans = Vec::new();
    let mut start_ms = 0;
    while start_ms < duration_ms {
        if spans.len() >= MAX_CHUNKS {
            return Err(TranscriptionError::InvalidChunkPlan);
        }
        let end_ms = start_ms.saturating_add(max_chunk_ms).min(duration_ms);
        spans.push((start_ms, end_ms));
        if end_ms == duration_ms {
            break;
        }
        start_ms = end_ms - overlap_ms;
    }

    Ok(spans
        .iter()
        .enumerate()
        .map(|(index, &(start_ms, end_ms))| {
            let core_start_ms = if index == 0 {
                start_ms
            } else {
                midpoint(start_ms, spans[index - 1].1)
            };
            let core_end_ms = if index + 1 == spans.len() {
                end_ms
            } else {
                midpoint(spans[index + 1].0, end_ms)
            };
            AudioChunk {
                index,
                start_ms,
                end_ms,
                core_start_ms,
                core_end_ms,
            }
        })
        .collect())
}

fn midpoint(a: u64, b: u64) -> u64 {
    a + (b - a) / 2
}

/// Build the documented Interactions API payload after the backend has
/// extracted a chunk into a supported audio format and uploaded it. WAV is a
/// suitable canonical extraction format for any imported audio track; the MIME
/// type must describe the extracted chunk, never the source video container.
/// The URI must be the Files API URI returned for that uploaded chunk.
/// Authentication is supplied by the backend HTTP adapter, never by the UI.
pub fn gemini_request(
    file_uri: &str,
    mime_type: &str,
) -> Result<serde_json::Value, TranscriptionError> {
    let file_id = file_uri.strip_prefix("https://generativelanguage.googleapis.com/v1beta/files/");
    if !file_id.is_some_and(|id| {
        !id.is_empty()
            && id.len() <= 256
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    }) || !matches!(
        mime_type,
        "audio/wav"
            | "audio/mp3"
            | "audio/aiff"
            | "audio/aac"
            | "audio/ogg"
            | "audio/flac"
            | "audio/mpeg"
            | "audio/m4a"
            | "audio/l16"
            | "audio/opus"
            | "audio/alaw"
            | "audio/mulaw"
            | "audio/webm"
    ) {
        return Err(TranscriptionError::InvalidResponse);
    }
    Ok(serde_json::json!({
        "model": "gemini-3.5-transcribe",
        "input": [{ "type": "audio", "uri": file_uri, "mime_type": mime_type }],
        "generation_config": {
            "transcription_config": {
                "mode": {
                    "type": "verbatim",
                    "diarization_mode": "speaker",
                    "timestamp_granularities": ["word"]
                }
            }
        }
    }))
}

#[derive(Deserialize)]
struct Interaction {
    status: String,
    #[serde(default)]
    output_text: Option<String>,
    #[serde(default)]
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    #[serde(default)]
    content: Vec<Content>,
}

#[derive(Deserialize)]
struct Content {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    annotations: Vec<Annotation>,
}

#[derive(Deserialize)]
struct Annotation {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
    speaker: Option<String>,
    start_offset: Option<String>,
    end_offset: Option<String>,
}

/// Parse a completed provider response, mapping relative word times to the
/// source timeline. Annotations outside this chunk's ownership range are
/// omitted so adjacent chunks do not repeat words from their shared overlap.
pub fn parse_gemini_chunk(
    response: &[u8],
    chunk: AudioChunk,
) -> Result<Vec<TranscriptSegment>, TranscriptionError> {
    if response.len() > MAX_RESPONSE_BYTES {
        return Err(TranscriptionError::ResponseTooLarge);
    }
    if chunk.start_ms >= chunk.end_ms
        || chunk.core_start_ms < chunk.start_ms
        || chunk.core_end_ms > chunk.end_ms
        || chunk.core_start_ms >= chunk.core_end_ms
    {
        return Err(TranscriptionError::InvalidChunkPlan);
    }
    let interaction: Interaction =
        serde_json::from_slice(response).map_err(|_| TranscriptionError::InvalidResponse)?;
    if interaction.status != "completed" {
        return Err(TranscriptionError::IncompleteResponse);
    }

    let explicitly_silent = interaction.output_text.as_deref() == Some("")
        && interaction
            .steps
            .iter()
            .flat_map(|step| &step.content)
            .filter_map(|content| content.text.as_deref())
            .all(str::is_empty);

    let mut words = Vec::new();
    let mut annotation_count = 0;
    let mut annotations = interaction
        .steps
        .iter()
        .flat_map(|step| &step.content)
        .flat_map(|content| &content.annotations)
        .filter(|annotation| annotation.kind == "word_info")
        .peekable();
    while let Some(annotation) = annotations.next() {
        annotation_count += 1;
        if annotation_count > MAX_WORDS_PER_CHUNK {
            return Err(TranscriptionError::InvalidResponse);
        }
        let text = annotation.text.as_deref().unwrap_or("").trim();
        if text.is_empty() || text.chars().any(char::is_control) {
            return Err(TranscriptionError::InvalidWord {
                index: annotation_count - 1,
                reason: "empty or control-character text",
            });
        }
        let mut start_ns = parse_offset_ns(annotation.start_offset.as_deref()).ok_or(
            TranscriptionError::InvalidWord {
                index: annotation_count - 1,
                reason: "invalid start offset",
            },
        )?;
        let mut end_ns = parse_offset_ns(annotation.end_offset.as_deref()).ok_or(
            TranscriptionError::InvalidWord {
                index: annotation_count - 1,
                reason: "invalid end offset",
            },
        )?;
        let chunk_duration_ms = chunk.end_ms - chunk.start_ms;
        let duration_ns = u128::from(chunk_duration_ms) * NANOS_PER_MILLISECOND;
        if start_ns == end_ns {
            // A provider point timestamp still locates the word. Give it a
            // short, positive caption interval, bounded by the next word's
            // start when that timestamp is usable.
            let next_start_ns = annotations
                .peek()
                .and_then(|next| parse_offset_ns(next.start_offset.as_deref()))
                .filter(|next| *next > start_ns);
            end_ns = start_ns
                .saturating_add(POINT_WORD_DURATION_NS)
                .min(duration_ns);
            if let Some(next_start_ns) = next_start_ns {
                end_ns = end_ns.min(next_start_ns);
            }
        } else if end_ns < start_ns && start_ns - end_ns <= MAX_REVERSED_WORD_NS {
            // Short reversed spans are an unambiguous local timestamp swap.
            std::mem::swap(&mut start_ns, &mut end_ns);
        }
        if end_ns < start_ns {
            return Err(TranscriptionError::ReversedWord {
                index: annotation_count - 1,
                start_offset_ms: rounded_milliseconds(start_ns).unwrap_or(u64::MAX),
                end_offset_ms: rounded_milliseconds(end_ns).unwrap_or(u64::MAX),
            });
        }
        if start_ns >= duration_ns || end_ns > duration_ns.saturating_add(MAX_END_OVERHANG_NS) {
            return Err(TranscriptionError::WordOutsideChunk {
                index: annotation_count - 1,
                end_offset_ms: u64::try_from(
                    end_ns.saturating_add(NANOS_PER_MILLISECOND / 2) / NANOS_PER_MILLISECOND,
                )
                .unwrap_or(u64::MAX),
                chunk_duration_ms,
            });
        }
        // Gemini offsets are relative to the uploaded WAV. Only a small end
        // overhang can be a boundary estimate; never move a wholly external
        // word into the audio or silently discard a large overhang.
        let bounded_end_ns = end_ns.min(duration_ns);
        let relative_middle_ns = start_ns + (bounded_end_ns - start_ns) / 2;
        let absolute_middle_ns =
            u128::from(chunk.start_ms) * NANOS_PER_MILLISECOND + relative_middle_ns;
        let core_start_ns = u128::from(chunk.core_start_ms) * NANOS_PER_MILLISECOND;
        let core_end_ns = u128::from(chunk.core_end_ms) * NANOS_PER_MILLISECOND;
        if (absolute_middle_ns < core_start_ns && chunk.core_start_ms > chunk.start_ms)
            || (absolute_middle_ns >= core_end_ns && chunk.core_end_ms < chunk.end_ms)
        {
            continue;
        }
        let mut start_ms = rounded_milliseconds(start_ns)?;
        let mut end_ms = rounded_milliseconds(bounded_end_ns)?.min(chunk_duration_ms);
        if start_ms >= end_ms {
            // A valid submillisecond word can round to a single millisecond.
            // Enclose its exact interval so the persisted caption remains
            // positive, without shifting it outside the extracted audio.
            start_ms = u64::try_from(start_ns / NANOS_PER_MILLISECOND)
                .map_err(|_| TranscriptionError::TimestampOverflow)?;
            end_ms = u64::try_from(
                bounded_end_ns.saturating_add(NANOS_PER_MILLISECOND - 1) / NANOS_PER_MILLISECOND,
            )
            .map_err(|_| TranscriptionError::TimestampOverflow)?
            .min(chunk_duration_ms);
            if start_ms >= end_ms {
                return Err(TranscriptionError::InvalidWord {
                    index: annotation_count - 1,
                    reason: "word has no representable duration",
                });
            }
        }
        let absolute_start = chunk
            .start_ms
            .checked_add(start_ms)
            .ok_or(TranscriptionError::TimestampOverflow)?;
        let absolute_end = chunk
            .start_ms
            .checked_add(end_ms)
            .ok_or(TranscriptionError::TimestampOverflow)?;
        let speaker = annotation
            .speaker
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if speaker.is_some_and(|s| s.len() > 64 || s.chars().any(char::is_control)) {
            return Err(TranscriptionError::InvalidWord {
                index: annotation_count - 1,
                reason: "invalid speaker",
            });
        }
        words.push(TranscriptSegment {
            start_seconds: absolute_start as f64 / 1000.0,
            end_seconds: absolute_end as f64 / 1000.0,
            text: text.to_owned(),
            speaker: speaker.map(str::to_owned),
        });
    }
    if annotation_count == 0 {
        return if explicitly_silent {
            Ok(Vec::new())
        } else {
            Err(TranscriptionError::MissingWordAnnotations)
        };
    }
    // Provider annotations may arrive in a different order than their valid
    // timestamps, especially after a local timestamp repair. Persistence
    // expects source-time order; keep each word's content and timing intact.
    words.sort_by(|left, right| left.start_seconds.total_cmp(&right.start_seconds));
    Ok(words)
}

/// Gemini offsets are decimal seconds suffixed with `s`, such as `0.100s`.
/// Preserve protobuf Duration's nanosecond precision until boundary checks.
fn parse_offset_ns(value: Option<&str>) -> Option<u128> {
    let raw = value?.strip_suffix('s')?;
    let (seconds, fractional) = raw.split_once('.').unwrap_or((raw, ""));
    if seconds.is_empty()
        || !seconds.bytes().all(|b| b.is_ascii_digit())
        || fractional.len() > 9
        || !fractional.bytes().all(|b| b.is_ascii_digit())
        || (raw.contains('.') && fractional.is_empty())
    {
        return None;
    }
    let whole = u128::from(seconds.parse::<u64>().ok()?).checked_mul(1_000_000_000)?;
    let fraction = if fractional.is_empty() {
        0
    } else {
        u128::from(fractional.parse::<u64>().ok()?) * 10u128.pow((9 - fractional.len()) as u32)
    };
    whole.checked_add(fraction)
}

fn rounded_milliseconds(nanoseconds: u128) -> Result<u64, TranscriptionError> {
    u64::try_from(nanoseconds.saturating_add(NANOS_PER_MILLISECOND / 2) / NANOS_PER_MILLISECOND)
        .map_err(|_| TranscriptionError::TimestampOverflow)
}

#[cfg(test)]
fn parse_offset(value: Option<&str>) -> Option<u64> {
    rounded_milliseconds(parse_offset_ns(value)?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(annotations: &str) -> Vec<u8> {
        format!(r#"{{"status":"completed","steps":[{{"type":"model_output","content":[{{"type":"text","annotations":[{annotations}]}}]}}]}}"#).into_bytes()
    }

    #[test]
    fn chunks_cover_source_and_share_overlap_at_midpoint() {
        let chunks = plan_chunks(70_000, 30_000, 2_000).unwrap();
        assert_eq!(chunks.len(), 3);
        assert_eq!(
            (chunks[0].start_ms, chunks[0].end_ms, chunks[0].core_end_ms),
            (0, 30_000, 29_000)
        );
        assert_eq!(
            (
                chunks[1].start_ms,
                chunks[1].core_start_ms,
                chunks[1].core_end_ms
            ),
            (28_000, 29_000, 57_000)
        );
        assert_eq!((chunks[2].start_ms, chunks[2].end_ms), (56_000, 70_000));
        assert_eq!(chunks[1].core_end_ms, chunks[2].core_start_ms);
    }

    #[test]
    fn source_offsets_and_overlap_ownership_are_applied() {
        let chunks = plan_chunks(70_000, 30_000, 2_000).unwrap();
        let first = response(
            r#"{"type":"word_info","text":"before","start_offset":"28.800s","end_offset":"29.000s"},{"type":"word_info","text":"after","start_offset":"29.100s","end_offset":"29.300s"}"#,
        );
        let second = response(
            r#"{"type":"word_info","text":"before","start_offset":"0.800s","end_offset":"1.000s"},{"type":"word_info","text":"after","speaker":"spk_1","start_offset":"1.100s","end_offset":"1.300s"}"#,
        );
        let mut words = parse_gemini_chunk(&first, chunks[0]).unwrap();
        words.extend(parse_gemini_chunk(&second, chunks[1]).unwrap());
        assert_eq!(
            words.iter().map(|w| w.text.as_str()).collect::<Vec<_>>(),
            ["before", "after"]
        );
        assert_eq!(words[1].start_seconds, 29.1);
        assert_eq!(words[1].speaker.as_deref(), Some("spk_1"));
    }

    #[test]
    fn bad_provider_times_are_rejected() {
        let chunk = plan_chunks(10_000, 10_000, 0).unwrap()[0];
        for (start, end) in [("-1s", "1s"), ("1.1234567890s", "2s"), ("1e2s", "2s")] {
            let raw = response(&format!(
                r#"{{"type":"word_info","text":"word","start_offset":"{start}","end_offset":"{end}"}}"#
            ));
            assert!(matches!(
                parse_gemini_chunk(&raw, chunk),
                Err(TranscriptionError::InvalidWord { .. })
            ));
        }
        let reversed =
            response(r#"{"type":"word_info","text":"word","start_offset":"2s","end_offset":"1s"}"#);
        assert_eq!(
            parse_gemini_chunk(&reversed, chunk),
            Err(TranscriptionError::ReversedWord {
                index: 0,
                start_offset_ms: 2_000,
                end_offset_ms: 1_000,
            })
        );
        let large_overhang = response(
            r#"{"type":"word_info","text":"word","start_offset":"9s","end_offset":"11s"}"#,
        );
        assert_eq!(
            parse_gemini_chunk(&large_overhang, chunk),
            Err(TranscriptionError::WordOutsideChunk {
                index: 0,
                end_offset_ms: 11_000,
                chunk_duration_ms: 10_000,
            })
        );
    }

    #[test]
    fn submillisecond_offsets_round_without_losing_source_alignment() {
        let chunk = plan_chunks(12_000, 10_000, 2_000).unwrap()[1];
        let raw = response(
            r#"{"type":"word_info","text":"fine","start_offset":"1.000499999s","end_offset":"1.999500001s"}"#,
        );
        let words = parse_gemini_chunk(&raw, chunk).unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].start_seconds, 9.0);
        assert_eq!(words[0].end_seconds, 10.0);
        assert_eq!(parse_offset(Some("1.9995s")), Some(2_000));
        assert_eq!(parse_offset(Some("0.000000001s")), Some(0));
    }

    #[test]
    fn twentieth_point_word_keeps_surrounding_words_and_uses_next_start() {
        let chunk = plan_chunks(10_000, 10_000, 0).unwrap()[0];
        let annotations: Vec<_> = (0..21)
            .map(|index| {
                let start_ms = 100 + index * 200;
                let end_ms = if index == 19 { start_ms } else { start_ms + 80 };
                serde_json::json!({
                    "type": "word_info", "text": format!("word{index}"),
                    "start_offset": format!("{}.{:03}s", start_ms / 1000, start_ms % 1000),
                    "end_offset": format!("{}.{:03}s", end_ms / 1000, end_ms % 1000),
                })
            })
            .collect();
        let raw = serde_json::to_vec(&serde_json::json!({
            "status": "completed", "steps": [{"content": [{"annotations": annotations}]}]
        }))
        .unwrap();
        let words = parse_gemini_chunk(&raw, chunk).unwrap();
        assert_eq!(words.len(), 21);
        assert_eq!(words[18].text, "word18");
        assert_eq!(words[19].text, "word19");
        assert_eq!((words[19].start_seconds, words[19].end_seconds), (3.9, 4.0));
        assert_eq!(words[20].text, "word20");
    }

    #[test]
    fn twentieth_short_reversal_is_swapped_without_discarding_words() {
        let chunk = plan_chunks(10_000, 10_000, 0).unwrap()[0];
        let annotations: Vec<_> = (0..21)
            .map(|index| {
                let start_ms = 100 + index * 200;
                let (start_ms, end_ms) = if index == 19 {
                    (start_ms + 80, start_ms)
                } else {
                    (start_ms, start_ms + 80)
                };
                serde_json::json!({
                    "type": "word_info", "text": format!("word{index}"),
                    "start_offset": format!("{}.{:03}s", start_ms / 1000, start_ms % 1000),
                    "end_offset": format!("{}.{:03}s", end_ms / 1000, end_ms % 1000),
                })
            })
            .collect();
        let raw = serde_json::to_vec(&serde_json::json!({
            "status": "completed", "steps": [{"content": [{"annotations": annotations}]}]
        }))
        .unwrap();
        let words = parse_gemini_chunk(&raw, chunk).unwrap();
        assert_eq!(words.len(), 21);
        assert_eq!(
            (words[19].start_seconds, words[19].end_seconds),
            (3.9, 3.98)
        );
        assert_eq!(words[18].text, "word18");
        assert_eq!(words[20].text, "word20");
    }

    #[test]
    fn submillisecond_boundary_is_checked_before_rounding() {
        let chunk = plan_chunks(1_000, 1_000, 0).unwrap()[0];
        let raw = response(
            r#"{"type":"word_info","text":"edge","start_offset":"0.900s","end_offset":"1.000499999s"}"#,
        );
        let words = parse_gemini_chunk(&raw, chunk).unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!((words[0].start_seconds, words[0].end_seconds), (0.9, 1.0));
    }

    #[test]
    fn small_end_overhang_is_clamped_but_wholly_external_word_is_rejected() {
        let chunk = plan_chunks(1_000, 1_000, 0).unwrap()[0];
        let raw = response(
            r#"{"type":"word_info","text":"edge","start_offset":"0.950s","end_offset":"1.050s"}"#,
        );
        let words = parse_gemini_chunk(&raw, chunk).unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!((words[0].start_seconds, words[0].end_seconds), (0.95, 1.0));

        let external = response(
            r#"{"type":"word_info","text":"invented","start_offset":"1.001s","end_offset":"1.030s"}"#,
        );
        assert!(matches!(
            parse_gemini_chunk(&external, chunk),
            Err(TranscriptionError::WordOutsideChunk { .. })
        ));
    }

    #[test]
    fn overlap_word_with_sane_small_overhang_belongs_to_adjacent_chunk() {
        let chunks = plan_chunks(19_000, 10_000, 2_000).unwrap();
        let raw = response(
            r#"{"type":"word_info","text":"next","start_offset":"9.900s","end_offset":"10.050s"}"#,
        );
        assert!(parse_gemini_chunk(&raw, chunks[0]).unwrap().is_empty());
        let too_far = response(
            r#"{"type":"word_info","text":"next","start_offset":"9.900s","end_offset":"11s"}"#,
        );
        assert!(matches!(
            parse_gemini_chunk(&too_far, chunks[0]),
            Err(TranscriptionError::WordOutsideChunk { .. })
        ));
    }

    #[test]
    fn effective_short_final_audio_bounds_last_chunk_without_losing_source_offset() {
        let planned = plan_chunks(12_000, 10_000, 2_000).unwrap()[1];
        let effective = AudioChunk {
            end_ms: planned.end_ms - 30,
            core_end_ms: planned.core_end_ms - 30,
            ..planned
        };
        let raw = response(
            r#"{"type":"word_info","text":"last","start_offset":"3.900s","end_offset":"4.020s"}"#,
        );
        let words = parse_gemini_chunk(&raw, effective).unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(
            (words[0].start_seconds, words[0].end_seconds),
            (11.9, 11.97)
        );
    }

    #[test]
    fn incomplete_or_untimed_response_is_not_a_usable_transcript() {
        let chunk = plan_chunks(10_000, 10_000, 0).unwrap()[0];
        assert_eq!(
            parse_gemini_chunk(br#"{"status":"in_progress"}"#, chunk),
            Err(TranscriptionError::IncompleteResponse)
        );
        assert_eq!(
            parse_gemini_chunk(&response(""), chunk),
            Err(TranscriptionError::MissingWordAnnotations)
        );
    }

    #[test]
    fn explicitly_empty_completed_output_is_a_silent_chunk() {
        let chunk = plan_chunks(10_000, 10_000, 0).unwrap()[0];
        let silent = br#"{"status":"completed","output_text":"","steps":[{"type":"model_output","content":[{"type":"text","text":"","annotations":[]}]}]}"#;
        assert_eq!(parse_gemini_chunk(silent, chunk).unwrap(), Vec::new());
        assert_eq!(
            parse_gemini_chunk(
                br#"{"status":"completed","output_text":"","steps":[]}"#,
                chunk
            )
            .unwrap(),
            Vec::new()
        );
        assert_eq!(
            parse_gemini_chunk(br#"{"status":"completed","steps":[]}"#, chunk),
            Err(TranscriptionError::MissingWordAnnotations)
        );
    }

    #[test]
    fn annotations_are_read_from_all_interaction_steps_and_content() {
        let chunk = plan_chunks(10_000, 10_000, 0).unwrap()[0];
        let annotated = br#"{"status":"completed","output_text":"hello","steps":[{"type":"other_output","content":[{"type":"other_content","annotations":[{"type":"word_info","text":"hello","start_offset":"0.1s","end_offset":"0.5s"}]}]}]}"#;
        let words = parse_gemini_chunk(annotated, chunk).unwrap();
        assert_eq!(words.len(), 1);
        assert_eq!(words[0].text, "hello");
        assert_eq!(words[0].start_seconds, 0.1);
        assert_eq!(words[0].end_seconds, 0.5);

        let untimed = br#"{"status":"completed","steps":[{"type":"other_output","content":[{"type":"other_content","annotations":[{"type":"word_info","text":"hello"}]}]}]}"#;
        assert_eq!(
            parse_gemini_chunk(untimed, chunk),
            Err(TranscriptionError::InvalidWord {
                index: 0,
                reason: "invalid start offset",
            })
        );
    }

    #[test]
    fn request_enables_documented_verbatim_word_and_speaker_mode() {
        let request = gemini_request(
            "https://generativelanguage.googleapis.com/v1beta/files/abc",
            "audio/mp3",
        )
        .unwrap();
        assert_eq!(
            request["generation_config"]["transcription_config"]["mode"]["timestamp_granularities"]
                [0],
            "word"
        );
        assert_eq!(
            request["generation_config"]["transcription_config"]["mode"]["diarization_mode"],
            "speaker"
        );
        for invalid_uri in [
            "file:///private/source.mp3",
            "https://generativelanguage.googleapis.com/v1beta/models/abc",
            "https://generativelanguage.googleapis.com/v1beta/files/abc/extra",
            "https://generativelanguage.googleapis.com/v1beta/files/abc?key=secret",
            "https://generativelanguage.googleapis.com/v1beta/files/",
        ] {
            assert!(gemini_request(invalid_uri, "audio/wav").is_err());
        }
        for supported_mime in ["audio/wav", "audio/aac", "audio/m4a", "audio/webm"] {
            assert!(
                gemini_request(
                    "https://generativelanguage.googleapis.com/v1beta/files/abc_123",
                    supported_mime
                )
                .is_ok()
            );
        }
        assert!(
            gemini_request(
                "https://generativelanguage.googleapis.com/v1beta/files/abc",
                "video/mp4"
            )
            .is_err()
        );
    }
}
