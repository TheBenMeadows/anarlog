use std::collections::HashMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use specta::Type;
use sqlx::SqlitePool;

use crate::{
    DEFAULT_LIST_LIMIT, Error, ListMeetingsInput, MAX_LIST_LIMIT, MeetingExport, Pagination,
    Result, Transcript, get_meeting_export, list_meetings, pagination, transcript_render,
};

pub const MAX_SEARCH_SNIPPETS: usize = 3;
const SNIPPET_WORDS_BEFORE: usize = 10;
const SNIPPET_WORDS_AFTER: usize = 20;

#[derive(
    Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Type, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub struct SearchMeetingsInput {
    #[schemars(
        description = "Case-insensitive word or phrase to find in transcripts, titles, notes, and summaries"
    )]
    pub query: String,
    #[schemars(
        description = "Folder path from list_folders, such as Projects/Launch; includes meetings in its subfolders"
    )]
    #[serde(default)]
    pub folder_path: Option<String>,
    #[schemars(description = "Maximum meetings; defaults to 20 and is capped at 200")]
    #[schemars(range(min = 1, max = 200))]
    pub limit: Option<u32>,
    #[schemars(description = "Number of matching meetings to skip; defaults to 0")]
    pub offset: Option<u32>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Type, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub struct MeetingSearchSnippet {
    /// `transcript`, `title`, `note`, or `summary`.
    pub field: String,
    pub text: String,
    /// Speaker of the first matched word; null outside transcripts or when the
    /// transcript has no speaker data.
    pub speaker: Option<String>,
    /// Time of the first matched word from the start of the recording.
    pub start_ms: Option<i64>,
    /// Transcript word offset where this snippet starts; pass it to
    /// get_meeting_transcript as `offset`.
    pub word_offset: Option<u32>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Type, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub struct MeetingSearchHit {
    pub meeting_id: String,
    pub title: String,
    pub started_at: String,
    pub folder_path: Option<String>,
    pub match_count: usize,
    pub matched_fields: Vec<String>,
    pub snippets: Vec<MeetingSearchSnippet>,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Type, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub struct MeetingSearchPage {
    pub hits: Vec<MeetingSearchHit>,
    pub pagination: Pagination,
}

#[derive(
    Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Type, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub struct TranscriptSegment {
    /// Null for words the stored speaker data does not cover.
    pub speaker: Option<String>,
    pub text: String,
    pub start_ms: Option<i64>,
}

pub async fn search_meetings(
    pool: &SqlitePool,
    input: SearchMeetingsInput,
) -> Result<MeetingSearchPage> {
    let tokens = query_tokens(&input.query);
    if tokens.is_empty() {
        return Err(Error::Invalid(
            "query must contain at least one letter or digit".to_string(),
        ));
    }
    let limit = input
        .limit
        .unwrap_or(DEFAULT_LIST_LIMIT)
        .clamp(1, MAX_LIST_LIMIT);
    let offset = input.offset.unwrap_or(0);

    let mut hits = Vec::new();
    let mut list_offset = 0;
    loop {
        let page = list_meetings(
            pool,
            ListMeetingsInput {
                query: None,
                series_id: None,
                folder_path: input.folder_path.clone(),
                limit: Some(MAX_LIST_LIMIT),
                offset: Some(list_offset),
            },
        )
        .await?;
        for meeting in &page.meetings {
            let export = match get_meeting_export(pool, meeting.id.clone()).await {
                Ok(export) => export,
                // Deleted between the listing and this read.
                Err(Error::NotFound(_)) => continue,
                Err(error) => return Err(error),
            };
            hits.extend(search_export(&export, &tokens));
        }
        match page.pagination.next_offset {
            Some(next) => list_offset = next,
            None => break,
        }
    }

    let total = hits.len();
    let hits = hits
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect::<Vec<_>>();
    let has_more = (offset as usize).saturating_add(hits.len()) < total;
    Ok(MeetingSearchPage {
        pagination: pagination(offset, limit, hits.len(), Some(total), has_more),
        hits,
    })
}

/// Groups a transcript page's consecutive words by speaker, using the labels
/// the desktop transcript view shows. Empty when the transcript has no
/// renderable speaker data.
pub(crate) fn page_segments(words: &[Value], export: &MeetingExport) -> Vec<TranscriptSegment> {
    let labels = transcript_render::speaker_labels_by_word_id(export);
    if labels.is_empty() {
        return Vec::new();
    }
    let offsets = transcript_offsets(&export.transcripts);

    let mut segments: Vec<TranscriptSegment> = Vec::new();
    let mut current_transcript: Option<&str> = None;
    for word in words {
        let Some(text) = word_text(word) else {
            continue;
        };
        let transcript_id = word.get("transcript_id").and_then(Value::as_str);
        let speaker = word_id(word).and_then(|id| labels.get(id)).cloned();
        let same_transcript = current_transcript == transcript_id;
        match segments.last_mut() {
            Some(segment)
                if same_transcript && (speaker.is_none() || segment.speaker == speaker) =>
            {
                segment.text.push(' ');
                segment.text.push_str(text);
            }
            _ => segments.push(TranscriptSegment {
                speaker,
                text: text.to_string(),
                start_ms: word_start_ms(word)
                    .map(|ms| ms + transcript_id.and_then(|id| offsets.get(id)).unwrap_or(&0)),
            }),
        }
        current_transcript = transcript_id;
    }
    segments
}

struct TranscriptWord<'a> {
    text: Option<&'a str>,
    id: Option<&'a str>,
    start_ms: Option<i64>,
}

fn search_export(export: &MeetingExport, tokens: &[String]) -> Option<MeetingSearchHit> {
    let mut match_count = 0;
    let mut matched_fields = Vec::new();
    let mut snippets = Vec::new();

    let words = transcript_words(&export.transcripts);
    let texts = words.iter().map(|word| word.text).collect::<Vec<_>>();
    let matches = field_matches(&texts, tokens);
    if !matches.is_empty() {
        match_count += matches.len();
        matched_fields.push("transcript".to_string());
        let labels = transcript_render::speaker_labels_by_word_id(export);
        for (window, first) in snippet_windows(&matches, texts.len(), tokens.len()) {
            if snippets.len() == MAX_SEARCH_SNIPPETS {
                break;
            }
            snippets.push(MeetingSearchSnippet {
                field: "transcript".to_string(),
                text: join_words(&texts[window.clone()]),
                speaker: words[first].id.and_then(|id| labels.get(id)).cloned(),
                start_ms: words[first].start_ms,
                word_offset: Some(window.start as u32),
            });
        }
    }

    let mut documents = vec![("title", export.meeting.title.as_str())];
    if let Some(note) = &export.meeting.note {
        documents.push(("note", note.markdown.as_str()));
    }
    for summary in &export.meeting.summaries {
        documents.push(("summary", summary.markdown.as_str()));
    }
    for (field, body) in documents {
        let texts = body.split_whitespace().map(Some).collect::<Vec<_>>();
        let matches = field_matches(&texts, tokens);
        if matches.is_empty() {
            continue;
        }
        match_count += matches.len();
        if !matched_fields.iter().any(|name| name == field) {
            matched_fields.push(field.to_string());
        }
        for (window, _) in snippet_windows(&matches, texts.len(), tokens.len()) {
            if snippets.len() == MAX_SEARCH_SNIPPETS {
                break;
            }
            snippets.push(MeetingSearchSnippet {
                field: field.to_string(),
                text: join_words(&texts[window]),
                speaker: None,
                start_ms: None,
                word_offset: None,
            });
        }
    }

    (match_count > 0).then(|| MeetingSearchHit {
        meeting_id: export.meeting.id.clone(),
        title: export.meeting.title.clone(),
        started_at: export.meeting.started_at.clone(),
        folder_path: export.meeting.folder_path.clone(),
        match_count,
        matched_fields,
        snippets,
    })
}

/// Every stored word in the order get_meeting_transcript pages them, so an
/// index here is a valid transcript `offset`.
fn transcript_words(transcripts: &[Transcript]) -> Vec<TranscriptWord<'_>> {
    let offsets = transcript_offsets(transcripts);
    transcripts
        .iter()
        .flat_map(|transcript| {
            let offset = offsets.get(transcript.id.as_str()).copied().unwrap_or(0);
            transcript.words.iter().map(move |word| TranscriptWord {
                text: word_text(word),
                id: word_id(word),
                start_ms: word_start_ms(word).map(|ms| ms + offset),
            })
        })
        .collect()
}

/// Each transcript's start relative to the earliest one, so word times read
/// from the start of the recording across pause/resume segments.
fn transcript_offsets(transcripts: &[Transcript]) -> HashMap<&str, i64> {
    let base = transcripts
        .iter()
        .map(|transcript| transcript.started_at_ms)
        .min()
        .unwrap_or(0);
    transcripts
        .iter()
        .map(|transcript| (transcript.id.as_str(), transcript.started_at_ms - base))
        .collect()
}

fn word_text(word: &Value) -> Option<&str> {
    word.get("text")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn word_id(word: &Value) -> Option<&str> {
    word.get("id").and_then(Value::as_str)
}

fn word_start_ms(word: &Value) -> Option<i64> {
    let start = word.get("start_ms")?;
    start
        .as_i64()
        .or_else(|| start.as_f64().map(|ms| ms.round() as i64))
}

fn normalize_token(raw: &str) -> String {
    raw.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

fn query_tokens(query: &str) -> Vec<String> {
    query
        .split_whitespace()
        .map(normalize_token)
        .filter(|token| !token.is_empty())
        .collect()
}

/// Indexes into `texts` of the first word of each phrase match. Matching is a
/// case-insensitive substring match over the words joined by single spaces,
/// with punctuation at word edges ignored.
fn field_matches(texts: &[Option<&str>], tokens: &[String]) -> Vec<usize> {
    let words = texts
        .iter()
        .enumerate()
        .filter_map(|(index, text)| {
            let word = normalize_token((*text)?);
            (!word.is_empty()).then_some((index, word))
        })
        .collect::<Vec<_>>();
    if words.len() < tokens.len() {
        return Vec::new();
    }
    let last = tokens.len() - 1;
    (0..=words.len() - tokens.len())
        .filter(|&start| {
            tokens.iter().enumerate().all(|(i, token)| {
                let word = &words[start + i].1;
                match (i == 0, i == last) {
                    (true, true) => word.contains(token.as_str()),
                    (true, false) => word.ends_with(token.as_str()),
                    (false, true) => word.starts_with(token.as_str()),
                    (false, false) => word == token,
                }
            })
        })
        .map(|start| words[start].0)
        .collect()
}

/// One window per match, skipping matches a previous window already shows.
/// Yields the window and the index of the match's first word.
fn snippet_windows(
    matches: &[usize],
    total: usize,
    phrase_len: usize,
) -> Vec<(std::ops::Range<usize>, usize)> {
    let mut windows: Vec<(std::ops::Range<usize>, usize)> = Vec::new();
    for &first in matches {
        if windows.last().is_some_and(|(window, _)| first < window.end) {
            continue;
        }
        let start = first.saturating_sub(SNIPPET_WORDS_BEFORE);
        let end = (first + phrase_len + SNIPPET_WORDS_AFTER).min(total);
        windows.push((start..end, first));
    }
    windows
}

fn join_words(texts: &[Option<&str>]) -> String {
    texts
        .iter()
        .flatten()
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASES: &str = include_str!("../testdata/search_cases.json");

    fn text<'a>(value: &'a Value, key: &str) -> &'a str {
        value.get(key).and_then(Value::as_str).unwrap_or("")
    }

    async fn seeded_db(seed: &Value) -> anlg_db_core::Db {
        let db = anlg_db_core::Db::connect_memory_plain().await.unwrap();
        anlg_db_app::prepare_schema(&db).await.unwrap();
        for human in seed["humans"].as_array().unwrap() {
            sqlx::query("INSERT INTO humans (id, name) VALUES (?, ?)")
                .bind(text(human, "id"))
                .bind(text(human, "name"))
                .execute(db.pool())
                .await
                .unwrap();
        }
        for meeting in seed["meetings"].as_array().unwrap() {
            let id = text(meeting, "id");
            sqlx::query(
                "INSERT INTO sessions (id, title, started_at, owner_user_id, folder_path)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(text(meeting, "title"))
            .bind(text(meeting, "started_at"))
            .bind(text(meeting, "owner_user_id"))
            .bind(text(meeting, "folder_path"))
            .execute(db.pool())
            .await
            .unwrap();
            for participant in meeting["participants"].as_array().into_iter().flatten() {
                let human_id = text(participant, "human_id");
                sqlx::query(
                    "INSERT INTO session_participants (id, session_id, human_id, display_name)
                     VALUES (?, ?, ?, ?)",
                )
                .bind(format!("{id}-{human_id}"))
                .bind(id)
                .bind(human_id)
                .bind(text(participant, "display_name"))
                .execute(db.pool())
                .await
                .unwrap();
            }
            for (kind, document_id) in [
                ("note", id.to_string()),
                ("summary", format!("{id}-summary")),
            ] {
                let Some(body) = meeting.get(kind).and_then(Value::as_str) else {
                    continue;
                };
                sqlx::query(
                    "INSERT INTO session_documents (id, session_id, kind, body_format, body)
                     VALUES (?, ?, ?, 'markdown', ?)",
                )
                .bind(document_id)
                .bind(id)
                .bind(kind)
                .bind(body)
                .execute(db.pool())
                .await
                .unwrap();
            }
            for transcript in meeting["transcripts"].as_array().unwrap() {
                sqlx::query(
                    "INSERT INTO transcripts
                     (id, session_id, owner_user_id, started_at_ms, words_json, speaker_hints_json)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(text(transcript, "id"))
                .bind(id)
                .bind(text(meeting, "owner_user_id"))
                .bind(transcript["started_at_ms"].as_i64().unwrap())
                .bind(transcript["words"].to_string())
                .bind(transcript["hints"].to_string())
                .execute(db.pool())
                .await
                .unwrap();
            }
        }
        db
    }

    fn assert_snippet(name: &str, actual: &MeetingSearchSnippet, expected: &Value) {
        let actual_json = serde_json::to_value(actual).unwrap();
        for (key, value) in expected.as_object().unwrap() {
            if key == "contains" {
                assert!(
                    actual.text.contains(value.as_str().unwrap()),
                    "{name}: snippet {:?} does not contain {value}",
                    actual.text
                );
            } else {
                assert_eq!(&actual_json[key], value, "{name}: snippet `{key}`");
            }
        }
    }

    #[tokio::test]
    async fn search_cases_file() {
        let file: Value = serde_json::from_str(CASES).unwrap();
        let db = seeded_db(&file["seed"]).await;
        let cases = file["cases"].as_array().unwrap();
        assert!(!cases.is_empty());

        for case in cases {
            let name = text(case, "name");
            let input: SearchMeetingsInput = serde_json::from_value(case["input"].clone()).unwrap();
            let result = search_meetings(db.pool(), input).await;
            let expect = &case["expect"];
            if expect.get("error").and_then(Value::as_bool) == Some(true) {
                assert!(
                    matches!(result, Err(Error::Invalid(_))),
                    "{name}: {result:?}"
                );
                continue;
            }
            let page = result.unwrap_or_else(|error| panic!("{name}: {error}"));

            assert_eq!(
                page.pagination.total,
                Some(expect["total"].as_u64().unwrap() as usize),
                "{name}: total"
            );
            if let Some(next_offset) = expect.get("next_offset") {
                assert_eq!(
                    page.pagination.next_offset.map(u64::from),
                    next_offset.as_u64(),
                    "{name}: next_offset"
                );
            }
            let expected_hits = expect["hits"].as_array().unwrap();
            assert_eq!(
                page.hits
                    .iter()
                    .map(|hit| hit.meeting_id.as_str())
                    .collect::<Vec<_>>(),
                expected_hits
                    .iter()
                    .map(|hit| text(hit, "meeting_id"))
                    .collect::<Vec<_>>(),
                "{name}: meetings"
            );
            for (hit, expected) in page.hits.iter().zip(expected_hits) {
                if let Some(count) = expected.get("match_count") {
                    assert_eq!(
                        hit.match_count as u64,
                        count.as_u64().unwrap(),
                        "{name}: count"
                    );
                }
                if let Some(fields) = expected.get("matched_fields") {
                    assert_eq!(
                        &serde_json::json!(hit.matched_fields),
                        fields,
                        "{name}: fields"
                    );
                }
                if let Some(snippets) = expected.get("snippets").and_then(Value::as_array) {
                    assert_eq!(hit.snippets.len(), snippets.len(), "{name}: snippet count");
                    for (snippet, expected) in hit.snippets.iter().zip(snippets) {
                        assert_snippet(name, snippet, expected);
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn transcript_page_groups_words_by_speaker() {
        let file: Value = serde_json::from_str(CASES).unwrap();
        let db = seeded_db(&file["seed"]).await;

        let page = crate::get_meeting_transcript(
            db.pool(),
            crate::GetMeetingTranscriptInput {
                meeting_id: "m1".to_string(),
                offset: None,
                limit: None,
            },
        )
        .await
        .unwrap();

        assert_eq!(page.text, "Hello there The parapet coping, leaks.");
        assert_eq!(
            page.segments,
            vec![
                TranscriptSegment {
                    speaker: Some("Speaker 1".to_string()),
                    text: "Hello there".to_string(),
                    start_ms: Some(0),
                },
                TranscriptSegment {
                    speaker: Some("Alice".to_string()),
                    text: "The parapet coping, leaks.".to_string(),
                    start_ms: Some(1000),
                },
            ]
        );
    }
}
