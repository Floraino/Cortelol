use crate::discovery::ValidatedSuggestion;
use crate::export::ClipRange;
use crate::media::{MediaInfo, probe_media};
use crate::transcription::{AudioChunk, TranscriptSegment, TranscriptionProvider};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;
use tauri::{AppHandle, Manager};

fn migrate(connection: &Connection) -> rusqlite::Result<()> {
    let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > 11 {
        return Err(rusqlite::Error::InvalidQuery);
    }
    if version == 0 {
        connection.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS recent_import (
                 id INTEGER PRIMARY KEY CHECK (id = 1),
                 source_path TEXT NOT NULL
             );
             PRAGMA user_version = 1;
             COMMIT;",
        )?;
    }
    if version < 2 {
        connection.execute_batch(
            "BEGIN;
             CREATE TABLE transcription_job (
                 source_path TEXT PRIMARY KEY,
                 duration_seconds REAL NOT NULL CHECK (duration_seconds > 0),
                 state TEXT NOT NULL CHECK (state IN ('idle', 'running', 'completed', 'cancelled', 'failed')),
                 stage TEXT NOT NULL,
                 error TEXT
             );
             CREATE TABLE transcription_chunk (
                 source_path TEXT NOT NULL REFERENCES transcription_job(source_path),
                 chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
                 start_ms INTEGER NOT NULL,
                 end_ms INTEGER NOT NULL,
                 core_start_ms INTEGER NOT NULL,
                 core_end_ms INTEGER NOT NULL,
                 completed INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
                 PRIMARY KEY (source_path, chunk_index),
                 CHECK (start_ms < end_ms AND start_ms <= core_start_ms AND
                        core_start_ms < core_end_ms AND core_end_ms <= end_ms)
             );
             CREATE TABLE transcript_segment (
                 id INTEGER PRIMARY KEY,
                 source_path TEXT NOT NULL,
                 chunk_index INTEGER NOT NULL,
                 original_start_seconds REAL NOT NULL,
                 original_end_seconds REAL NOT NULL,
                 original_text TEXT NOT NULL,
                 speaker TEXT,
                 corrected_start_seconds REAL,
                 corrected_end_seconds REAL,
                 corrected_text TEXT,
                 FOREIGN KEY (source_path, chunk_index)
                     REFERENCES transcription_chunk(source_path, chunk_index)
             );
             CREATE INDEX transcript_segment_source_order
                 ON transcript_segment(source_path, original_start_seconds, id);
             PRAGMA user_version = 2;
             COMMIT;",
        )?;
    }
    if version < 3 {
        connection.execute_batch(
            "BEGIN;
             CREATE TABLE discovery_job (
                 source_path TEXT NOT NULL,
                 generation INTEGER NOT NULL CHECK (generation > 0),
                 provider TEXT NOT NULL CHECK (provider IN ('claude', 'openai')),
                 model TEXT NOT NULL,
                 transcript_revision TEXT NOT NULL,
                 state TEXT NOT NULL CHECK (state IN ('idle', 'running', 'completed', 'cancelled', 'failed')),
                 stage TEXT NOT NULL,
                 error TEXT,
                 total_windows INTEGER NOT NULL CHECK (total_windows > 0),
                 PRIMARY KEY (source_path, generation),
                 FOREIGN KEY (source_path) REFERENCES transcription_job(source_path)
             );
             CREATE TABLE discovery_window (
                 source_path TEXT NOT NULL,
                 generation INTEGER NOT NULL,
                 window_index INTEGER NOT NULL CHECK (window_index >= 0),
                 completed INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
                 PRIMARY KEY (source_path, generation, window_index),
                 FOREIGN KEY (source_path, generation)
                     REFERENCES discovery_job(source_path, generation)
             );
             CREATE TABLE discovery_suggestion (
                 id INTEGER PRIMARY KEY,
                 source_path TEXT NOT NULL,
                 generation INTEGER NOT NULL,
                 window_index INTEGER NOT NULL,
                 original_start_seconds REAL NOT NULL,
                 original_end_seconds REAL NOT NULL,
                 edited_start_seconds REAL,
                 edited_end_seconds REAL,
                 hook TEXT NOT NULL,
                 rationale TEXT NOT NULL,
                 source_quote TEXT NOT NULL,
                 score REAL,
                 review_decision TEXT NOT NULL DEFAULT 'pending'
                     CHECK (review_decision IN ('pending', 'accepted', 'rejected')),
                 CHECK (original_start_seconds >= 0 AND original_end_seconds > original_start_seconds),
                 CHECK (edited_start_seconds IS NULL OR edited_start_seconds >= 0),
                 CHECK (edited_end_seconds IS NULL OR edited_end_seconds > edited_start_seconds),
                 CHECK (score IS NULL OR (score >= 0 AND score <= 1)),
                 FOREIGN KEY (source_path, generation, window_index)
                     REFERENCES discovery_window(source_path, generation, window_index)
             );
             CREATE INDEX discovery_suggestion_job_order
                 ON discovery_suggestion(source_path, generation, window_index, id);
             PRAGMA user_version = 3;
             COMMIT;",
        )?;
    }
    if version < 4 {
        connection.execute_batch(
            "BEGIN;
             ALTER TABLE transcription_job ADD COLUMN source_size_bytes INTEGER;
             ALTER TABLE transcription_job ADD COLUMN source_modified_ns TEXT;
             PRAGMA user_version = 4;
             COMMIT;",
        )?;
    }
    if version < 5 {
        connection.execute_batch(
            "BEGIN;
             CREATE TABLE clip_draft (
                 source_path TEXT PRIMARY KEY,
                 source_size_bytes INTEGER NOT NULL CHECK (source_size_bytes > 0),
                 source_modified_ns TEXT NOT NULL,
                 start_ms INTEGER NOT NULL CHECK (start_ms >= 0),
                 end_ms INTEGER NOT NULL CHECK (end_ms > start_ms),
                 framing_mode TEXT NOT NULL CHECK (framing_mode IN ('crop', 'fit')),
                 horizontal_percent INTEGER NOT NULL CHECK (horizontal_percent BETWEEN 0 AND 100),
                 reviewed INTEGER NOT NULL DEFAULT 0 CHECK (reviewed IN (0, 1)),
                 review_session TEXT
             );
             CREATE TABLE clip_caption (
                 source_path TEXT NOT NULL REFERENCES clip_draft(source_path),
                 cue_index INTEGER NOT NULL CHECK (cue_index >= 0),
                 start_ms INTEGER NOT NULL CHECK (start_ms >= 0),
                 end_ms INTEGER NOT NULL CHECK (end_ms > start_ms),
                 text TEXT NOT NULL,
                 PRIMARY KEY (source_path, cue_index)
             );
             CREATE TABLE completed_export (
                 source_path TEXT PRIMARY KEY,
                 source_size_bytes INTEGER NOT NULL CHECK (source_size_bytes > 0),
                 source_modified_ns TEXT NOT NULL,
                 export_path TEXT NOT NULL,
                 export_sha256 TEXT NOT NULL
             );
             PRAGMA user_version = 5;
             COMMIT;",
        )?;
    }
    if version < 6 {
        // SQLite cannot widen a CHECK constraint in place. Preserve the composite
        // parent key so existing windows, suggestions, and reviews keep their IDs.
        let foreign_keys: i64 =
            connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        connection.pragma_update(None, "foreign_keys", 0)?;
        let result = (|| {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(
                "CREATE TABLE discovery_job_v6 (
                    source_path TEXT NOT NULL,
                    generation INTEGER NOT NULL CHECK (generation > 0),
                    provider TEXT NOT NULL CHECK (provider IN ('claude', 'openai', 'deepseek')),
                    model TEXT NOT NULL,
                    transcript_revision TEXT NOT NULL,
                    state TEXT NOT NULL CHECK (state IN ('idle', 'running', 'completed', 'cancelled', 'failed')),
                    stage TEXT NOT NULL,
                    error TEXT,
                    total_windows INTEGER NOT NULL CHECK (total_windows > 0),
                    PRIMARY KEY (source_path, generation),
                    FOREIGN KEY (source_path) REFERENCES transcription_job(source_path)
                );
                INSERT INTO discovery_job_v6
                    (source_path, generation, provider, model, transcript_revision,
                     state, stage, error, total_windows)
                    SELECT source_path, generation, provider, model, transcript_revision,
                           state, stage, error, total_windows FROM discovery_job;
                DROP TABLE discovery_job;
                ALTER TABLE discovery_job_v6 RENAME TO discovery_job;",
            )?;
            let invalid_references: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check",
                [],
                |row| row.get(0),
            )?;
            if invalid_references != 0 {
                return Err(rusqlite::Error::InvalidQuery);
            }
            transaction.pragma_update(None, "user_version", 6)?;
            transaction.commit()
        })();
        connection.pragma_update(None, "foreign_keys", foreign_keys)?;
        result?;
    }
    if version < 7 {
        connection.execute_batch(
            "BEGIN;
             ALTER TABLE transcription_job ADD COLUMN provider TEXT NOT NULL DEFAULT 'gemini'
                 CHECK (provider IN ('gemini', 'elevenlabs'));
             PRAGMA user_version = 7;
             COMMIT;",
        )?;
    }
    if version < 8 {
        connection.execute_batch(
            "BEGIN;
             ALTER TABLE transcription_chunk ADD COLUMN provider TEXT
                 CHECK (provider IN ('gemini', 'elevenlabs'));
             UPDATE transcription_chunk SET provider = 'gemini' WHERE completed = 1;
             PRAGMA user_version = 8;
             COMMIT;",
        )?;
    }
    if version < 9 {
        connection.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS project_framing_preference (
                 source_path TEXT PRIMARY KEY,
                 mode TEXT NOT NULL CHECK (mode IN ('local', 'gemini'))
             );
             PRAGMA user_version = 9;
             COMMIT;",
        )?;
    }
    if version < 10 {
        connection.execute_batch(
            "BEGIN;
             ALTER TABLE clip_draft ADD COLUMN framing_keyframes TEXT NOT NULL DEFAULT '[]';
             PRAGMA user_version = 10;
             COMMIT;",
        )?;
    }
    if version < 11 {
        // Widen the provider checks while retaining transcript IDs, corrections,
        // discovery references, and the completed chunk provenance.
        let foreign_keys: i64 =
            connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
        connection.pragma_update(None, "foreign_keys", 0)?;
        let result = (|| {
            let transaction = connection.unchecked_transaction()?;
            transaction.execute_batch(
                "CREATE TABLE transcription_job_v11 (
                    source_path TEXT PRIMARY KEY,
                    duration_seconds REAL NOT NULL CHECK (duration_seconds > 0),
                    state TEXT NOT NULL CHECK (state IN ('idle', 'running', 'completed', 'cancelled', 'failed')),
                    stage TEXT NOT NULL,
                    error TEXT,
                    source_size_bytes INTEGER,
                    source_modified_ns TEXT,
                    provider TEXT NOT NULL DEFAULT 'gemini'
                        CHECK (provider IN ('gemini', 'elevenlabs', 'local'))
                );
                CREATE TABLE transcription_chunk_v11 (
                    source_path TEXT NOT NULL REFERENCES transcription_job_v11(source_path),
                    chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
                    start_ms INTEGER NOT NULL,
                    end_ms INTEGER NOT NULL,
                    core_start_ms INTEGER NOT NULL,
                    core_end_ms INTEGER NOT NULL,
                    completed INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
                    provider TEXT CHECK (provider IN ('gemini', 'elevenlabs', 'local')),
                    PRIMARY KEY (source_path, chunk_index),
                    CHECK (start_ms < end_ms AND start_ms <= core_start_ms AND
                           core_start_ms < core_end_ms AND core_end_ms <= end_ms)
                );
                INSERT INTO transcription_job_v11
                    (source_path, duration_seconds, state, stage, error,
                     source_size_bytes, source_modified_ns, provider)
                    SELECT source_path, duration_seconds, state, stage, error,
                           source_size_bytes, source_modified_ns, provider
                    FROM transcription_job;
                INSERT INTO transcription_chunk_v11
                    (source_path, chunk_index, start_ms, end_ms, core_start_ms,
                     core_end_ms, completed, provider)
                    SELECT source_path, chunk_index, start_ms, end_ms, core_start_ms,
                           core_end_ms, completed, provider
                    FROM transcription_chunk;
                DROP TABLE transcription_chunk;
                DROP TABLE transcription_job;
                ALTER TABLE transcription_job_v11 RENAME TO transcription_job;
                ALTER TABLE transcription_chunk_v11 RENAME TO transcription_chunk;",
            )?;
            let invalid_references: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_check",
                [],
                |row| row.get(0),
            )?;
            if invalid_references != 0 {
                return Err(rusqlite::Error::InvalidQuery);
            }
            transaction.pragma_update(None, "user_version", 11)?;
            transaction.commit()
        })();
        connection.pragma_update(None, "foreign_keys", foreign_keys)?;
        result?;
    }
    Ok(())
}

fn open_database(app: &AppHandle) -> Result<Connection, String> {
    let directory = app
        .path()
        .app_data_dir()
        .map_err(|_| "The project data directory is unavailable.".to_string())?;
    fs::create_dir_all(&directory)
        .map_err(|_| "The project data directory could not be created.".to_string())?;
    let connection = Connection::open(directory.join("projects.sqlite"))
        .map_err(|_| "The project database could not be opened.".to_string())?;
    prepare_database(&connection)
        .map_err(|_| "The project database could not be updated.".to_string())?;
    Ok(connection)
}

fn prepare_database(connection: &Connection) -> rusqlite::Result<()> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    migrate(connection)
}

fn save_recent(connection: &Connection, path: &str) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO recent_import (id, source_path) VALUES (1, ?1)
         ON CONFLICT(id) DO UPDATE SET source_path = excluded.source_path",
        params![path],
    )?;
    Ok(())
}

fn recent_path(connection: &Connection) -> rusqlite::Result<Option<String>> {
    connection
        .query_row(
            "SELECT source_path FROM recent_import WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .optional()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FramingAnalysisMode {
    Local,
    Gemini,
}

impl FramingAnalysisMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Gemini => "gemini",
        }
    }
}

fn read_framing_analysis_mode(
    connection: &Connection,
    source_path: &str,
) -> rusqlite::Result<FramingAnalysisMode> {
    let saved: Option<String> = connection
        .query_row(
            "SELECT mode FROM project_framing_preference WHERE source_path = ?1",
            [source_path],
            |row| row.get(0),
        )
        .optional()?;
    Ok(if saved.as_deref() == Some("gemini") {
        FramingAnalysisMode::Gemini
    } else {
        FramingAnalysisMode::Local
    })
}

fn save_framing_analysis_mode(
    connection: &Connection,
    source_path: &str,
    mode: FramingAnalysisMode,
) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO project_framing_preference (source_path, mode) VALUES (?1, ?2)
         ON CONFLICT(source_path) DO UPDATE SET mode = excluded.mode",
        params![source_path, mode.as_str()],
    )?;
    Ok(())
}

#[tauri::command]
pub fn get_framing_analysis_mode(app: AppHandle) -> Result<FramingAnalysisMode, String> {
    let connection = open_database(&app)?;
    let source_path = recent_path(&connection)
        .map_err(|_| "The recent project could not be read.".to_string())?
        .ok_or_else(|| "Import a recording before choosing framing analysis.".to_string())?;
    read_framing_analysis_mode(&connection, &source_path)
        .map_err(|_| "The framing preference could not be read.".to_string())
}

#[tauri::command]
pub fn set_framing_analysis_mode(app: AppHandle, mode: FramingAnalysisMode) -> Result<(), String> {
    let connection = open_database(&app)?;
    let source_path = recent_path(&connection)
        .map_err(|_| "The recent project could not be read.".to_string())?
        .ok_or_else(|| "Import a recording before choosing framing analysis.".to_string())?;
    save_framing_analysis_mode(&connection, &source_path, mode)
        .map_err(|_| "The framing preference could not be saved.".to_string())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TranscriptionState {
    Idle,
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl TranscriptionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    fn from_str(value: &str) -> rusqlite::Result<Self> {
        match value {
            "idle" => Ok(Self::Idle),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredTranscriptionChunk {
    pub index: usize,
    pub start_ms: u64,
    pub end_ms: u64,
    pub core_start_ms: u64,
    pub core_end_ms: u64,
    pub completed: bool,
    pub provider: Option<TranscriptionProvider>,
}

impl StoredTranscriptionChunk {
    pub fn audio_chunk(&self) -> AudioChunk {
        AudioChunk {
            index: self.index,
            start_ms: self.start_ms,
            end_ms: self.end_ms,
            core_start_ms: self.core_start_ms,
            core_end_ms: self.core_end_ms,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredTranscriptSegment {
    pub id: i64,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub text: String,
    pub speaker: Option<String>,
    pub corrected: bool,
    pub provider: Option<TranscriptionProvider>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TranscriptionJob {
    pub provider: TranscriptionProvider,
    pub state: TranscriptionState,
    pub stage: String,
    pub error: Option<String>,
    pub completed_chunks: usize,
    pub total_chunks: usize,
    pub chunks: Vec<StoredTranscriptionChunk>,
    pub segments: Vec<StoredTranscriptSegment>,
}

fn validated_source(path: &str) -> Result<String, String> {
    let canonical = Path::new(path)
        .canonicalize()
        .map_err(|_| "The recording could not be found.".to_string())?;
    if !canonical.is_file() {
        return Err("The recording is not a file.".to_string());
    }
    Ok(canonical.to_string_lossy().into_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceIdentity {
    size_bytes: i64,
    modified_ns: String,
}

fn source_identity(source_path: &str) -> Result<SourceIdentity, String> {
    let metadata =
        fs::metadata(source_path).map_err(|_| "The recording could not be read.".to_string())?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err("The recording is not a readable media file.".to_string());
    }
    let size_bytes = i64::try_from(metadata.len())
        .map_err(|_| "The recording is too large to track safely.".to_string())?;
    let modified_ns = metadata
        .modified()
        .map_err(|_| "The recording modification time is unavailable.".to_string())?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| "The recording modification time is invalid.".to_string())?
        .as_nanos()
        .to_string();
    Ok(SourceIdentity {
        size_bytes,
        modified_ns,
    })
}

fn ensure_source_identity(connection: &Connection, source_path: &str) -> Result<(), String> {
    let saved: Option<(Option<i64>, Option<String>)> = connection
        .query_row(
            "SELECT source_size_bytes, source_modified_ns FROM transcription_job
             WHERE source_path = ?1",
            params![source_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| "The recording identity could not be read.".to_string())?;
    let Some((size_bytes, modified_ns)) = saved else {
        return Ok(());
    };
    let current = source_identity(source_path)?;
    match (size_bytes, modified_ns) {
        (Some(size), Some(modified))
            if size == current.size_bytes && modified == current.modified_ns => Ok(()),
        (None, None) => Err("This transcript predates source identity tracking. Import a separate copy of the recording to transcribe it safely.".to_string()),
        _ => Err("The recording changed since this transcript was created. Import a separate copy before transcribing or editing it.".to_string()),
    }
}

fn valid_segment(segment: &TranscriptSegment, duration_seconds: f64) -> bool {
    segment.start_seconds.is_finite()
        && segment.end_seconds.is_finite()
        && segment.start_seconds >= 0.0
        && segment.start_seconds < segment.end_seconds
        && segment.end_seconds <= duration_seconds + 0.001
        && !segment.text.trim().is_empty()
        && segment.text.len() <= 16_384
        && segment.speaker.as_ref().is_none_or(|s| s.len() <= 256)
}

fn read_job(
    connection: &Connection,
    source_path: &str,
) -> rusqlite::Result<Option<TranscriptionJob>> {
    let header: Option<(String, String, Option<String>, String)> = connection
        .query_row(
            "SELECT state, stage, error, provider FROM transcription_job WHERE source_path = ?1",
            params![source_path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((state, stage, error, provider)) = header else {
        return Ok(None);
    };
    let provider =
        TranscriptionProvider::from_str(&provider).ok_or(rusqlite::Error::InvalidQuery)?;
    let mut chunk_statement = connection.prepare(
        "SELECT chunk_index, start_ms, end_ms, core_start_ms, core_end_ms, completed, provider
         FROM transcription_chunk WHERE source_path = ?1 ORDER BY chunk_index",
    )?;
    let chunks = chunk_statement
        .query_map(params![source_path], |row| {
            Ok(StoredTranscriptionChunk {
                index: row.get::<_, i64>(0)? as usize,
                start_ms: row.get::<_, i64>(1)? as u64,
                end_ms: row.get::<_, i64>(2)? as u64,
                core_start_ms: row.get::<_, i64>(3)? as u64,
                core_end_ms: row.get::<_, i64>(4)? as u64,
                completed: row.get::<_, i64>(5)? != 0,
                provider: row
                    .get::<_, Option<String>>(6)?
                    .map(|provider| {
                        TranscriptionProvider::from_str(&provider)
                            .ok_or(rusqlite::Error::InvalidQuery)
                    })
                    .transpose()?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut segment_statement = connection.prepare(
        "SELECT s.id, COALESCE(s.corrected_start_seconds, s.original_start_seconds),
                COALESCE(s.corrected_end_seconds, s.original_end_seconds),
                COALESCE(s.corrected_text, s.original_text), s.speaker,
                s.corrected_start_seconds IS NOT NULL OR s.corrected_end_seconds IS NOT NULL
                    OR s.corrected_text IS NOT NULL,
                c.provider
         FROM transcript_segment s
         LEFT JOIN transcription_chunk c ON c.source_path = s.source_path
             AND c.chunk_index = s.chunk_index
         WHERE s.source_path = ?1
         ORDER BY COALESCE(s.corrected_start_seconds, s.original_start_seconds), s.id",
    )?;
    let segments = segment_statement
        .query_map(params![source_path], |row| {
            Ok(StoredTranscriptSegment {
                id: row.get(0)?,
                start_seconds: row.get(1)?,
                end_seconds: row.get(2)?,
                text: row.get(3)?,
                speaker: row.get(4)?,
                corrected: row.get::<_, i64>(5)? != 0,
                provider: row
                    .get::<_, Option<String>>(6)?
                    .map(|provider| {
                        TranscriptionProvider::from_str(&provider)
                            .ok_or(rusqlite::Error::InvalidQuery)
                    })
                    .transpose()?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let completed_chunks = chunks.iter().filter(|chunk| chunk.completed).count();
    Ok(Some(TranscriptionJob {
        provider,
        state: TranscriptionState::from_str(&state)?,
        stage,
        error,
        completed_chunks,
        total_chunks: chunks.len(),
        chunks,
        segments,
    }))
}

fn create_job(
    connection: &mut Connection,
    source_path: &str,
    duration_seconds: f64,
    chunks: &[AudioChunk],
    identity: Option<&SourceIdentity>,
    provider: TranscriptionProvider,
) -> Result<TranscriptionJob, String> {
    if !duration_seconds.is_finite()
        || duration_seconds <= 0.0
        || chunks.is_empty()
        || chunks.len() > 10_000
    {
        return Err("The transcription plan is invalid.".to_string());
    }
    let duration_ms = (duration_seconds * 1000.0).ceil() as u64;
    for (index, chunk) in chunks.iter().enumerate() {
        if chunk.index != index
            || chunk.start_ms >= chunk.end_ms
            || chunk.core_start_ms < chunk.start_ms
            || chunk.core_start_ms >= chunk.core_end_ms
            || chunk.core_end_ms > chunk.end_ms
            || chunk.end_ms > duration_ms
            || (index == 0 && chunk.core_start_ms != 0)
            || (index > 0 && chunks[index - 1].core_end_ms != chunk.core_start_ms)
        {
            return Err("The transcription plan is invalid.".to_string());
        }
    }
    if chunks
        .last()
        .is_none_or(|chunk| chunk.core_end_ms != duration_ms)
    {
        return Err("The transcription plan does not cover the recording.".to_string());
    }
    let transaction = connection
        .transaction()
        .map_err(|_| "The transcription job could not be opened.".to_string())?;
    let existing_job: Option<(f64, Option<i64>, Option<String>, String)> = transaction
        .query_row(
            "SELECT duration_seconds, source_size_bytes, source_modified_ns, provider
             FROM transcription_job WHERE source_path = ?1",
            params![source_path],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|_| "The transcription job could not be read.".to_string())?;
    if let Some((existing_duration, saved_size, saved_modified, saved_provider)) = existing_job {
        if let Some(identity) = identity {
            if saved_size != Some(identity.size_bytes)
                || saved_modified.as_deref() != Some(&identity.modified_ns)
            {
                return Err("The recording changed or its saved identity is unavailable. Import a separate copy before transcribing it.".to_string());
            }
        }
        if (existing_duration - duration_seconds).abs() > 0.001 {
            return Err("The recording duration changed since transcription began.".to_string());
        }
        let existing: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM transcription_chunk WHERE source_path = ?1",
                params![source_path],
                |row| row.get(0),
            )
            .map_err(|_| "The transcription plan could not be read.".to_string())?;
        if existing != chunks.len() as i64 {
            return Err("The saved transcription plan differs from the current plan.".to_string());
        }
        for chunk in chunks {
            let matches: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM transcription_chunk WHERE source_path = ?1
                     AND chunk_index = ?2 AND start_ms = ?3 AND end_ms = ?4
                     AND core_start_ms = ?5 AND core_end_ms = ?6",
                    params![
                        source_path,
                        chunk.index as i64,
                        chunk.start_ms as i64,
                        chunk.end_ms as i64,
                        chunk.core_start_ms as i64,
                        chunk.core_end_ms as i64
                    ],
                    |row| row.get(0),
                )
                .map_err(|_| "The transcription plan could not be read.".to_string())?;
            if matches != 1 {
                return Err(
                    "The saved transcription plan differs from the current plan.".to_string(),
                );
            }
        }
        if saved_provider != provider.as_str() {
            let pending: i64 = transaction
                .query_row(
                    "SELECT COUNT(*) FROM transcription_chunk
                     WHERE source_path = ?1 AND completed = 0",
                    params![source_path],
                    |row| row.get(0),
                )
                .map_err(|_| "The saved transcription progress could not be read.".to_string())?;
            if pending == 0 {
                return Err("All chunks of this recording are already transcribed. Import a separate copy to create a new transcript with another provider while preserving this one.".to_string());
            }
            transaction
                .execute(
                    "UPDATE transcription_job
                     SET provider = ?2, state = 'idle',
                         stage = 'Ready to transcribe remaining chunks', error = NULL
                     WHERE source_path = ?1",
                    params![source_path, provider.as_str()],
                )
                .map_err(|_| "The transcription provider could not be changed.".to_string())?;
        }
    } else {
        transaction
            .execute(
                "INSERT INTO transcription_job
                 (source_path, duration_seconds, state, stage, source_size_bytes, source_modified_ns, provider)
                 VALUES (?1, ?2, 'idle', 'Ready to transcribe', ?3, ?4, ?5)",
                params![source_path, duration_seconds, identity.map(|value| value.size_bytes), identity.map(|value| &value.modified_ns), provider.as_str()],
            )
            .map_err(|_| "The transcription job could not be created.".to_string())?;
        for chunk in chunks {
            transaction
                .execute(
                    "INSERT INTO transcription_chunk
                     (source_path, chunk_index, start_ms, end_ms, core_start_ms, core_end_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        source_path,
                        chunk.index as i64,
                        chunk.start_ms as i64,
                        chunk.end_ms as i64,
                        chunk.core_start_ms as i64,
                        chunk.core_end_ms as i64
                    ],
                )
                .map_err(|_| "The transcription plan could not be saved.".to_string())?;
        }
    }
    transaction
        .execute(
            "UPDATE transcription_job SET state = 'failed',
             stage = 'Interrupted; resume to continue',
             error = 'Transcription was interrupted before completion.'
             WHERE source_path = ?1 AND state = 'running'",
            params![source_path],
        )
        .map_err(|_| "The transcription job could not be recovered.".to_string())?;
    transaction
        .commit()
        .map_err(|_| "The transcription job could not be saved.".to_string())?;
    read_job(connection, source_path)
        .map_err(|_| "The transcription job could not be read.".to_string())?
        .ok_or_else(|| "The transcription job could not be found.".to_string())
}

pub(crate) fn load_or_create_transcription_job(
    app: &AppHandle,
    media: &MediaInfo,
    chunks: &[AudioChunk],
    provider: TranscriptionProvider,
) -> Result<TranscriptionJob, String> {
    let source = validated_source(&media.path)?;
    let identity = source_identity(&source)?;
    if identity.size_bytes as u64 != media.file_size_bytes {
        return Err("The recording changed while preparing transcription.".to_string());
    }
    let mut connection = open_database(app)?;
    create_job(
        &mut connection,
        &source,
        media.duration_seconds,
        chunks,
        Some(&identity),
        provider,
    )
}

pub(crate) fn load_transcription_job(
    app: &AppHandle,
    source_path: &str,
) -> Result<Option<TranscriptionJob>, String> {
    let source = validated_source(source_path)?;
    let connection = open_database(app)?;
    ensure_source_identity(&connection, &source)?;
    read_job(&connection, &source)
        .map_err(|_| "The transcription job could not be read.".to_string())
}

fn complete_chunk(
    connection: &mut Connection,
    source_path: &str,
    chunk_index: usize,
    segments: &[TranscriptSegment],
    provider: TranscriptionProvider,
) -> Result<TranscriptionJob, String> {
    let transaction = connection
        .transaction()
        .map_err(|_| "The transcription result could not be saved.".to_string())?;
    let chunk: Option<(i64, i64, i64, f64, String)> = transaction
        .query_row(
            "SELECT c.core_start_ms, c.core_end_ms, c.completed, j.duration_seconds, j.provider
             FROM transcription_chunk c JOIN transcription_job j USING (source_path)
             WHERE c.source_path = ?1 AND c.chunk_index = ?2",
            params![source_path, chunk_index as i64],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| "The transcription chunk could not be read.".to_string())?;
    let Some((core_start_ms, core_end_ms, completed, duration_seconds, active_provider)) = chunk
    else {
        return Err("The transcription chunk does not exist.".to_string());
    };
    if active_provider != provider.as_str() {
        return Err("The transcription provider changed before this chunk could be saved. Retry with the selected provider.".to_string());
    }
    if completed != 0 {
        return read_job(&transaction, source_path)
            .map_err(|_| "The transcription job could not be read.".to_string())?
            .ok_or_else(|| "The transcription job could not be found.".to_string());
    }
    let mut previous_start = -1.0;
    for segment in segments {
        let midpoint_ms = (segment.start_seconds + segment.end_seconds) * 500.0;
        if !valid_segment(segment, duration_seconds)
            || midpoint_ms < core_start_ms as f64 - 0.001
            || midpoint_ms >= core_end_ms as f64 + 0.001
            || segment.start_seconds < previous_start
        {
            return Err("The transcription contains invalid or unsorted timestamps.".to_string());
        }
        previous_start = segment.start_seconds;
    }
    for segment in segments {
        transaction
            .execute(
                "INSERT INTO transcript_segment
                 (source_path, chunk_index, original_start_seconds,
                  original_end_seconds, original_text, speaker)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    source_path,
                    chunk_index as i64,
                    segment.start_seconds,
                    segment.end_seconds,
                    segment.text,
                    segment.speaker
                ],
            )
            .map_err(|_| "The transcription result could not be saved.".to_string())?;
    }
    transaction
        .execute(
            "UPDATE transcription_chunk SET completed = 1, provider = ?3
             WHERE source_path = ?1 AND chunk_index = ?2",
            params![source_path, chunk_index as i64, provider.as_str()],
        )
        .map_err(|_| "The transcription progress could not be saved.".to_string())?;
    transaction
        .execute(
            "UPDATE transcription_job SET state = CASE
                WHEN (SELECT COUNT(*) FROM transcription_chunk WHERE source_path = ?1 AND completed = 0) = 0
                THEN 'completed' ELSE 'running' END,
             stage = CASE
                WHEN (SELECT COUNT(*) FROM transcription_chunk WHERE source_path = ?1 AND completed = 0) = 0
                THEN 'Transcript ready' ELSE 'Transcribing' END,
             error = NULL WHERE source_path = ?1",
            params![source_path],
        )
        .map_err(|_| "The transcription progress could not be saved.".to_string())?;
    transaction
        .commit()
        .map_err(|_| "The transcription result could not be saved.".to_string())?;
    read_job(connection, source_path)
        .map_err(|_| "The transcription job could not be read.".to_string())?
        .ok_or_else(|| "The transcription job could not be found.".to_string())
}

pub(crate) fn complete_transcription_chunk(
    app: &AppHandle,
    source_path: &str,
    chunk_index: usize,
    segments: &[TranscriptSegment],
    provider: TranscriptionProvider,
) -> Result<TranscriptionJob, String> {
    let source = validated_source(source_path)?;
    let mut connection = open_database(app)?;
    ensure_source_identity(&connection, &source)?;
    complete_chunk(&mut connection, &source, chunk_index, segments, provider)
}

pub(crate) fn set_transcription_status(
    app: &AppHandle,
    source_path: &str,
    state: TranscriptionState,
    stage: &str,
    error: Option<&str>,
) -> Result<TranscriptionJob, String> {
    if stage.trim().is_empty() || stage.len() > 256 || error.is_some_and(|e| e.len() > 2048) {
        return Err("The transcription status is invalid.".to_string());
    }
    let source = validated_source(source_path)?;
    let connection = open_database(app)?;
    ensure_source_identity(&connection, &source)?;
    let changed = connection
        .execute(
            "UPDATE transcription_job SET state = ?2, stage = ?3, error = ?4 WHERE source_path = ?1",
            params![source, state.as_str(), stage, error],
        )
        .map_err(|_| "The transcription status could not be saved.".to_string())?;
    if changed == 0 {
        return Err("The transcription job does not exist.".to_string());
    }
    read_job(&connection, &source)
        .map_err(|_| "The transcription job could not be read.".to_string())?
        .ok_or_else(|| "The transcription job could not be found.".to_string())
}

fn edit_segment(
    connection: &Connection,
    source_path: &str,
    id: i64,
    text: &str,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<TranscriptionJob, String> {
    let duration: Option<f64> = connection
        .query_row(
            "SELECT duration_seconds FROM transcription_job WHERE source_path = ?1",
            params![source_path],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "The recording duration could not be read.".to_string())?;
    let Some(duration) = duration else {
        return Err("The transcription job does not exist.".to_string());
    };
    let candidate = TranscriptSegment {
        start_seconds,
        end_seconds,
        text: text.to_string(),
        speaker: None,
    };
    if !valid_segment(&candidate, duration) {
        return Err("The corrected transcript segment is invalid.".to_string());
    }
    let changed = connection
        .execute(
            "UPDATE transcript_segment SET corrected_start_seconds = ?3,
             corrected_end_seconds = ?4, corrected_text = ?5
             WHERE source_path = ?1 AND id = ?2",
            params![source_path, id, start_seconds, end_seconds, text],
        )
        .map_err(|_| "The transcript correction could not be saved.".to_string())?;
    if changed == 0 {
        return Err("The transcript segment does not exist.".to_string());
    }
    read_job(connection, source_path)
        .map_err(|_| "The transcription job could not be read.".to_string())?
        .ok_or_else(|| "The transcription job could not be found.".to_string())
}

pub(crate) fn update_transcript_segment(
    app: &AppHandle,
    source_path: &str,
    id: i64,
    text: &str,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<TranscriptionJob, String> {
    let source = validated_source(source_path)?;
    let connection = open_database(app)?;
    ensure_source_identity(&connection, &source)?;
    edit_segment(&connection, &source, id, text, start_seconds, end_seconds)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DiscoveryState {
    Idle,
    Running,
    Completed,
    Cancelled,
    Failed,
}

impl DiscoveryState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    fn from_str(value: &str) -> rusqlite::Result<Self> {
        match value {
            "idle" => Ok(Self::Idle),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ReviewDecision {
    Pending,
    Accepted,
    Rejected,
}

impl ReviewDecision {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
        }
    }

    fn from_str(value: &str) -> rusqlite::Result<Self> {
        match value {
            "pending" => Ok(Self::Pending),
            "accepted" => Ok(Self::Accepted),
            "rejected" => Ok(Self::Rejected),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredDiscoveryWindow {
    pub index: usize,
    pub completed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredDiscoverySuggestion {
    pub id: i64,
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub original_start_seconds: f64,
    pub original_end_seconds: f64,
    pub hook: String,
    pub rationale: String,
    pub source_quote: String,
    pub score: Option<f64>,
    pub review_decision: ReviewDecision,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoveryJob {
    pub state: DiscoveryState,
    pub stage: String,
    pub error: Option<String>,
    pub provider: String,
    pub model: String,
    pub transcript_revision: String,
    pub completed_windows: usize,
    pub total_windows: usize,
    pub windows: Vec<StoredDiscoveryWindow>,
    pub suggestions: Vec<StoredDiscoverySuggestion>,
}

fn read_discovery_job(
    connection: &Connection,
    source_path: &str,
) -> rusqlite::Result<Option<DiscoveryJob>> {
    let header: Option<(
        i64,
        String,
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
    )> = connection
        .query_row(
            "SELECT generation, provider, model, transcript_revision, state, stage,
                        error, total_windows
                 FROM discovery_job WHERE source_path = ?1 ORDER BY generation DESC LIMIT 1",
            params![source_path],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()?;
    let Some((generation, provider, model, transcript_revision, state, stage, error, total)) =
        header
    else {
        return Ok(None);
    };
    let mut window_statement = connection.prepare(
        "SELECT window_index, completed FROM discovery_window
         WHERE source_path = ?1 AND generation = ?2 ORDER BY window_index",
    )?;
    let windows = window_statement
        .query_map(params![source_path, generation], |row| {
            Ok(StoredDiscoveryWindow {
                index: row.get::<_, i64>(0)? as usize,
                completed: row.get::<_, i64>(1)? != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut suggestion_statement = connection.prepare(
        "SELECT id, COALESCE(edited_start_seconds, original_start_seconds),
                COALESCE(edited_end_seconds, original_end_seconds),
                original_start_seconds, original_end_seconds, hook, rationale,
                source_quote, score, review_decision
         FROM discovery_suggestion WHERE source_path = ?1 AND generation = ?2
         ORDER BY window_index, id",
    )?;
    let suggestions = suggestion_statement
        .query_map(params![source_path, generation], |row| {
            Ok(StoredDiscoverySuggestion {
                id: row.get(0)?,
                start_seconds: row.get(1)?,
                end_seconds: row.get(2)?,
                original_start_seconds: row.get(3)?,
                original_end_seconds: row.get(4)?,
                hook: row.get(5)?,
                rationale: row.get(6)?,
                source_quote: row.get(7)?,
                score: row.get(8)?,
                review_decision: ReviewDecision::from_str(&row.get::<_, String>(9)?)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(DiscoveryJob {
        state: DiscoveryState::from_str(&state)?,
        stage,
        error,
        provider,
        model,
        transcript_revision,
        completed_windows: windows.iter().filter(|window| window.completed).count(),
        total_windows: total as usize,
        windows,
        suggestions,
    }))
}

fn discovery_generation(
    connection: &Connection,
    source_path: &str,
) -> rusqlite::Result<Option<i64>> {
    connection.query_row(
        "SELECT MAX(generation) FROM discovery_job WHERE source_path = ?1",
        params![source_path],
        |row| row.get(0),
    )
}

pub(crate) fn load_discovery_job(
    app: &AppHandle,
    source_path: &str,
) -> Result<Option<DiscoveryJob>, String> {
    let source = validated_source(source_path)?;
    let connection = open_database(app)?;
    ensure_source_identity(&connection, &source)?;
    read_discovery_job(&connection, &source)
        .map_err(|_| "The discovery job could not be read.".to_string())
}

fn create_discovery_job(
    connection: &mut Connection,
    source_path: &str,
    provider: &str,
    model: &str,
    transcript_revision: &str,
    window_count: usize,
) -> Result<DiscoveryJob, String> {
    if !matches!(provider, "claude" | "openai" | "deepseek")
        || model.trim().is_empty()
        || model.len() > 128
        || model.chars().any(char::is_control)
        || transcript_revision.len() != 64
        || !transcript_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || window_count == 0
        || window_count > 1_000
    {
        return Err("The discovery plan is invalid.".to_string());
    }
    let transaction = connection
        .transaction()
        .map_err(|_| "The discovery job could not be opened.".to_string())?;
    let transcript_state: Option<String> = transaction
        .query_row(
            "SELECT state FROM transcription_job WHERE source_path = ?1",
            params![source_path],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "The transcript could not be read.".to_string())?;
    if transcript_state.as_deref() != Some("completed") {
        return Err("Complete transcription before discovering clips.".to_string());
    }
    let latest: Option<(i64, String, String, String, i64)> = transaction
        .query_row(
            "SELECT generation, provider, model, transcript_revision, total_windows
             FROM discovery_job WHERE source_path = ?1 ORDER BY generation DESC LIMIT 1",
            params![source_path],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()
        .map_err(|_| "The discovery job could not be read.".to_string())?;
    let same_plan =
        latest
            .as_ref()
            .is_some_and(|(_, old_provider, old_model, old_revision, old_total)| {
                old_provider == provider
                    && old_model == model
                    && old_revision == transcript_revision
                    && *old_total == window_count as i64
            });
    if same_plan {
        transaction
            .execute(
                "UPDATE discovery_job SET state = 'failed',
                 stage = 'Interrupted; resume to continue',
                 error = 'Discovery was interrupted before completion.'
                 WHERE source_path = ?1 AND generation = ?2 AND state = 'running'",
                params![source_path, latest.as_ref().unwrap().0],
            )
            .map_err(|_| "The discovery job could not be recovered.".to_string())?;
    } else {
        let generation = latest.map_or(1, |(previous, ..)| previous + 1);
        transaction
            .execute(
                "INSERT INTO discovery_job
                 (source_path, generation, provider, model, transcript_revision, state, stage, total_windows)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'idle', 'Ready to discover clips', ?6)",
                params![source_path, generation, provider, model, transcript_revision, window_count as i64],
            )
            .map_err(|_| "The discovery job could not be created.".to_string())?;
        for index in 0..window_count {
            transaction
                .execute(
                    "INSERT INTO discovery_window (source_path, generation, window_index)
                     VALUES (?1, ?2, ?3)",
                    params![source_path, generation, index as i64],
                )
                .map_err(|_| "The discovery plan could not be saved.".to_string())?;
        }
    }
    transaction
        .commit()
        .map_err(|_| "The discovery job could not be saved.".to_string())?;
    read_discovery_job(connection, source_path)
        .map_err(|_| "The discovery job could not be read.".to_string())?
        .ok_or_else(|| "The discovery job could not be found.".to_string())
}

pub(crate) fn create_or_resume_discovery_job(
    app: &AppHandle,
    source_path: &str,
    provider: &str,
    model: &str,
    transcript_revision: &str,
    window_count: usize,
) -> Result<DiscoveryJob, String> {
    let source = validated_source(source_path)?;
    let mut connection = open_database(app)?;
    ensure_source_identity(&connection, &source)?;
    create_discovery_job(
        &mut connection,
        &source,
        provider,
        model,
        transcript_revision,
        window_count,
    )
}

fn append_discovery_result(
    connection: &mut Connection,
    source_path: &str,
    window_index: usize,
    suggestions: &[ValidatedSuggestion],
) -> Result<DiscoveryJob, String> {
    if suggestions.len() > 20 {
        return Err("The discovery result contains too many suggestions.".to_string());
    }
    let transaction = connection
        .transaction()
        .map_err(|_| "The discovery result could not be saved.".to_string())?;
    let Some(generation) = discovery_generation(&transaction, source_path)
        .map_err(|_| "The discovery job could not be read.".to_string())?
    else {
        return Err("The discovery job does not exist.".to_string());
    };
    let window: Option<(i64, f64)> = transaction
        .query_row(
            "SELECT w.completed, t.duration_seconds FROM discovery_window w
             JOIN transcription_job t USING (source_path)
             WHERE w.source_path = ?1 AND w.generation = ?2 AND w.window_index = ?3",
            params![source_path, generation, window_index as i64],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| "The discovery window could not be read.".to_string())?;
    let Some((completed, duration)) = window else {
        return Err("The discovery window does not exist.".to_string());
    };
    if completed != 0 {
        return read_discovery_job(&transaction, source_path)
            .map_err(|_| "The discovery job could not be read.".to_string())?
            .ok_or_else(|| "The discovery job could not be found.".to_string());
    }
    for suggestion in suggestions {
        if !suggestion.start_seconds.is_finite()
            || !suggestion.end_seconds.is_finite()
            || suggestion.start_seconds < 0.0
            || suggestion.end_seconds <= suggestion.start_seconds
            || suggestion.end_seconds > duration + 0.001
            || [
                &suggestion.hook,
                &suggestion.rationale,
                &suggestion.source_quote,
            ]
            .iter()
            .any(|text| text.trim().is_empty() || text.len() > 2_000)
            || suggestion
                .score
                .is_some_and(|score| !score.is_finite() || !(0.0..=1.0).contains(&score))
        {
            return Err("The discovery result contains invalid suggestions.".to_string());
        }
    }
    for suggestion in suggestions {
        let mut existing = transaction
            .prepare(
                "SELECT original_start_seconds, original_end_seconds
                 FROM discovery_suggestion WHERE source_path = ?1 AND generation = ?2
                 AND original_start_seconds < ?4 AND original_end_seconds > ?3",
            )
            .map_err(|_| "Existing discovery suggestions could not be checked.".to_string())?;
        let duplicates = existing
            .query_map(
                params![
                    source_path,
                    generation,
                    suggestion.start_seconds,
                    suggestion.end_seconds
                ],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?)),
            )
            .map_err(|_| "Existing discovery suggestions could not be checked.".to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|_| "Existing discovery suggestions could not be checked.".to_string())?
            .into_iter()
            .any(|(start, end)| {
                let intersection = (suggestion.end_seconds.min(end)
                    - suggestion.start_seconds.max(start))
                .max(0.0);
                let union = suggestion.end_seconds.max(end) - suggestion.start_seconds.min(start);
                union > 0.0 && intersection / union >= 0.85
            });
        drop(existing);
        if duplicates {
            continue;
        }
        transaction
            .execute(
                "INSERT INTO discovery_suggestion
                 (source_path, generation, window_index, original_start_seconds,
                  original_end_seconds, hook, rationale, source_quote, score)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    source_path,
                    generation,
                    window_index as i64,
                    suggestion.start_seconds,
                    suggestion.end_seconds,
                    suggestion.hook,
                    suggestion.rationale,
                    suggestion.source_quote,
                    suggestion.score
                ],
            )
            .map_err(|_| "The discovery suggestion could not be saved.".to_string())?;
    }
    transaction
        .execute(
            "UPDATE discovery_window SET completed = 1
             WHERE source_path = ?1 AND generation = ?2 AND window_index = ?3",
            params![source_path, generation, window_index as i64],
        )
        .map_err(|_| "The discovery progress could not be saved.".to_string())?;
    transaction
        .execute(
            "UPDATE discovery_job SET
             state = CASE WHEN (SELECT COUNT(*) FROM discovery_window
                 WHERE source_path = ?1 AND generation = ?2 AND completed = 0) = 0
                 THEN 'completed' ELSE 'running' END,
             stage = CASE WHEN (SELECT COUNT(*) FROM discovery_window
                 WHERE source_path = ?1 AND generation = ?2 AND completed = 0) = 0
                 THEN 'Suggestions ready' ELSE 'Discovering clips' END,
             error = NULL WHERE source_path = ?1 AND generation = ?2",
            params![source_path, generation],
        )
        .map_err(|_| "The discovery progress could not be saved.".to_string())?;
    transaction
        .commit()
        .map_err(|_| "The discovery result could not be saved.".to_string())?;
    read_discovery_job(connection, source_path)
        .map_err(|_| "The discovery job could not be read.".to_string())?
        .ok_or_else(|| "The discovery job could not be found.".to_string())
}

pub(crate) fn append_discovery_window(
    app: &AppHandle,
    source_path: &str,
    window_index: usize,
    suggestions: &[ValidatedSuggestion],
) -> Result<DiscoveryJob, String> {
    let source = validated_source(source_path)?;
    let mut connection = open_database(app)?;
    ensure_source_identity(&connection, &source)?;
    append_discovery_result(&mut connection, &source, window_index, suggestions)
}

fn update_discovery_status(
    connection: &Connection,
    source_path: &str,
    state: DiscoveryState,
    stage: &str,
    error: Option<&str>,
) -> Result<DiscoveryJob, String> {
    if stage.trim().is_empty()
        || stage.len() > 256
        || error.is_some_and(|error| error.len() > 2_048)
    {
        return Err("The discovery status is invalid.".to_string());
    }
    let changed = connection
        .execute(
            "UPDATE discovery_job SET state = ?2, stage = ?3, error = ?4
             WHERE source_path = ?1 AND generation =
                 (SELECT MAX(generation) FROM discovery_job WHERE source_path = ?1)",
            params![source_path, state.as_str(), stage, error],
        )
        .map_err(|_| "The discovery status could not be saved.".to_string())?;
    if changed == 0 {
        return Err("The discovery job does not exist.".to_string());
    }
    read_discovery_job(connection, source_path)
        .map_err(|_| "The discovery job could not be read.".to_string())?
        .ok_or_else(|| "The discovery job could not be found.".to_string())
}

pub(crate) fn set_discovery_status(
    app: &AppHandle,
    source_path: &str,
    state: DiscoveryState,
    stage: &str,
    error: Option<&str>,
) -> Result<DiscoveryJob, String> {
    let source = validated_source(source_path)?;
    let connection = open_database(app)?;
    ensure_source_identity(&connection, &source)?;
    update_discovery_status(&connection, &source, state, stage, error)
}

fn review_suggestion(
    connection: &Connection,
    source_path: &str,
    id: i64,
    decision: ReviewDecision,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<DiscoveryJob, String> {
    let duration: Option<f64> = connection
        .query_row(
            "SELECT duration_seconds FROM transcription_job WHERE source_path = ?1",
            params![source_path],
            |row| row.get(0),
        )
        .optional()
        .map_err(|_| "The recording duration could not be read.".to_string())?;
    let Some(duration) = duration else {
        return Err("The transcript does not exist.".to_string());
    };
    if !start_seconds.is_finite()
        || !end_seconds.is_finite()
        || start_seconds < 0.0
        || end_seconds <= start_seconds
        || end_seconds > duration + 0.001
    {
        return Err("The reviewed clip boundaries are invalid.".to_string());
    }
    let changed = connection
        .execute(
            "UPDATE discovery_suggestion SET review_decision = ?3,
             edited_start_seconds = ?4, edited_end_seconds = ?5
             WHERE source_path = ?1 AND id = ?2 AND generation =
                 (SELECT MAX(generation) FROM discovery_job WHERE source_path = ?1)",
            params![
                source_path,
                id,
                decision.as_str(),
                start_seconds,
                end_seconds
            ],
        )
        .map_err(|_| "The suggestion review could not be saved.".to_string())?;
    if changed == 0 {
        return Err("The suggestion does not exist in the current discovery job.".to_string());
    }
    read_discovery_job(connection, source_path)
        .map_err(|_| "The discovery job could not be read.".to_string())?
        .ok_or_else(|| "The discovery job could not be found.".to_string())
}

pub(crate) fn review_discovery_suggestion(
    app: &AppHandle,
    source_path: &str,
    id: i64,
    decision: ReviewDecision,
    start_seconds: f64,
    end_seconds: f64,
) -> Result<DiscoveryJob, String> {
    let source = validated_source(source_path)?;
    let connection = open_database(app)?;
    ensure_source_identity(&connection, &source)?;
    review_suggestion(
        &connection,
        &source,
        id,
        decision,
        start_seconds,
        end_seconds,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum StoredFramingMode {
    Crop,
    Fit,
}

impl StoredFramingMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Crop => "crop",
            Self::Fit => "fit",
        }
    }

    fn from_str(value: &str) -> rusqlite::Result<Self> {
        match value {
            "crop" => Ok(Self::Crop),
            "fit" => Ok(Self::Fit),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredCaptionCue {
    pub start_ms: u64,
    pub end_ms: u64,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredFramingKeyframe {
    pub timestamp_ms: u64,
    pub mode: StoredFramingMode,
    pub horizontal_percent: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredClipDraft {
    pub start_ms: u64,
    pub end_ms: u64,
    pub framing_mode: StoredFramingMode,
    pub horizontal_percent: u8,
    #[serde(default)]
    pub framing_keyframes: Vec<StoredFramingKeyframe>,
    pub captions: Vec<StoredCaptionCue>,
    #[serde(default)]
    pub reviewed: bool,
}

fn review_session() -> &'static str {
    static SESSION: OnceLock<String> = OnceLock::new();
    SESSION.get_or_init(|| {
        let started_ns = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        format!("{}-{started_ns}", std::process::id())
    })
}

fn clip_draft_is_valid(draft: &StoredClipDraft, duration_seconds: f64) -> bool {
    ClipRange {
        start_ms: draft.start_ms,
        end_ms: draft.end_ms,
    }
    .validate(duration_seconds)
    .is_ok()
        && i64::try_from(draft.start_ms).is_ok()
        && i64::try_from(draft.end_ms).is_ok()
        && draft.horizontal_percent <= 100
        && draft.framing_keyframes.len() <= 150
        && (draft.framing_keyframes.is_empty()
            || draft.framing_keyframes[0].timestamp_ms == draft.start_ms)
        && draft
            .framing_keyframes
            .iter()
            .enumerate()
            .all(|(index, keyframe)| {
                keyframe.timestamp_ms >= draft.start_ms
                    && keyframe.timestamp_ms < draft.end_ms
                    && keyframe.horizontal_percent <= 100
                    && i64::try_from(keyframe.timestamp_ms).is_ok()
                    && (index == 0
                        || keyframe.timestamp_ms > draft.framing_keyframes[index - 1].timestamp_ms)
            })
        && draft.captions.len() <= 500
        && draft.captions.iter().all(|cue| {
            cue.start_ms < cue.end_ms
                && i64::try_from(cue.start_ms).is_ok()
                && i64::try_from(cue.end_ms).is_ok()
                && cue.end_ms as f64 <= duration_seconds * 1_000.0 + 0.001
                && !cue.text.trim().is_empty()
                && cue.text.chars().count() <= 240
        })
}

fn read_clip_draft(
    connection: &Connection,
    source_path: &str,
    identity: &SourceIdentity,
) -> Result<Option<StoredClipDraft>, String> {
    let header: Option<(
        i64,
        String,
        i64,
        i64,
        String,
        i64,
        String,
        i64,
        Option<String>,
    )> = connection
        .query_row(
            "SELECT source_size_bytes, source_modified_ns, start_ms, end_ms,
                    framing_mode, horizontal_percent, framing_keyframes, reviewed, review_session
             FROM clip_draft WHERE source_path = ?1",
            params![source_path],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .optional()
        .map_err(|_| "The clip draft could not be read.".to_string())?;
    let Some((size, modified, start, end, mode, horizontal, keyframes, reviewed, session)) = header
    else {
        return Ok(None);
    };
    if size != identity.size_bytes || modified != identity.modified_ns {
        return Err("The recording changed since this clip draft was saved. Import a separate copy before editing it.".to_string());
    }
    let mut statement = connection
        .prepare(
            "SELECT start_ms, end_ms, text FROM clip_caption
             WHERE source_path = ?1 ORDER BY cue_index",
        )
        .map_err(|_| "The saved captions could not be read.".to_string())?;
    let captions = statement
        .query_map(params![source_path], |row| {
            Ok(StoredCaptionCue {
                start_ms: row.get::<_, i64>(0)? as u64,
                end_ms: row.get::<_, i64>(1)? as u64,
                text: row.get(2)?,
            })
        })
        .map_err(|_| "The saved captions could not be read.".to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|_| "The saved captions could not be read.".to_string())?;
    let active_review = reviewed != 0 && session.as_deref() == Some(review_session());
    if reviewed != 0 && !active_review {
        connection
            .execute(
                "UPDATE clip_draft SET reviewed = 0, review_session = NULL WHERE source_path = ?1",
                params![source_path],
            )
            .map_err(|_| "The clip review state could not be reset.".to_string())?;
    }
    Ok(Some(StoredClipDraft {
        start_ms: start as u64,
        end_ms: end as u64,
        framing_mode: StoredFramingMode::from_str(&mode)
            .map_err(|_| "The saved framing mode is invalid.".to_string())?,
        horizontal_percent: u8::try_from(horizontal)
            .map_err(|_| "The saved framing position is invalid.".to_string())?,
        framing_keyframes: serde_json::from_str(&keyframes)
            .map_err(|_| "The saved framing keyframes are invalid.".to_string())?,
        captions,
        reviewed: active_review,
    }))
}

pub(crate) fn load_clip_draft(
    app: &AppHandle,
    source_path: &str,
) -> Result<Option<StoredClipDraft>, String> {
    let source = validated_source(source_path)?;
    let identity = source_identity(&source)?;
    let connection = open_database(app)?;
    ensure_source_identity(&connection, &source)?;
    read_clip_draft(&connection, &source, &identity)
}

fn persist_clip_draft(
    connection: &mut Connection,
    source_path: &str,
    identity: &SourceIdentity,
    duration_seconds: f64,
    draft: &StoredClipDraft,
) -> Result<StoredClipDraft, String> {
    if !clip_draft_is_valid(draft, duration_seconds) {
        return Err("The clip draft has invalid timing, framing, or captions.".to_string());
    }
    ensure_source_identity(connection, source_path)?;
    // A changed source must never silently overwrite a draft for the old bytes.
    let existing: Option<(i64, String)> = connection
        .query_row(
            "SELECT source_size_bytes, source_modified_ns FROM clip_draft WHERE source_path = ?1",
            params![source_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|_| "The clip draft could not be checked.".to_string())?;
    if existing.is_some_and(|(size, modified)| {
        size != identity.size_bytes || modified != identity.modified_ns
    }) {
        return Err("The recording changed since this clip draft was saved. Import a separate copy before editing it.".to_string());
    }
    let transaction = connection
        .transaction()
        .map_err(|_| "The clip draft could not be saved.".to_string())?;
    transaction
        .execute(
            "INSERT INTO clip_draft
             (source_path, source_size_bytes, source_modified_ns, start_ms, end_ms,
              framing_mode, horizontal_percent, framing_keyframes, reviewed, review_session)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, NULL)
             ON CONFLICT(source_path) DO UPDATE SET
              start_ms = excluded.start_ms, end_ms = excluded.end_ms,
              framing_mode = excluded.framing_mode,
              horizontal_percent = excluded.horizontal_percent,
              framing_keyframes = excluded.framing_keyframes,
              reviewed = 0, review_session = NULL",
            params![
                source_path,
                identity.size_bytes,
                identity.modified_ns,
                draft.start_ms as i64,
                draft.end_ms as i64,
                draft.framing_mode.as_str(),
                draft.horizontal_percent as i64,
                serde_json::to_string(&draft.framing_keyframes)
                    .map_err(|_| "The framing keyframes could not be saved.".to_string())?,
            ],
        )
        .map_err(|_| "The clip draft could not be saved.".to_string())?;
    transaction
        .execute(
            "DELETE FROM clip_caption WHERE source_path = ?1",
            params![source_path],
        )
        .map_err(|_| "The saved captions could not be updated.".to_string())?;
    for (index, cue) in draft.captions.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO clip_caption (source_path, cue_index, start_ms, end_ms, text)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    source_path,
                    index as i64,
                    cue.start_ms as i64,
                    cue.end_ms as i64,
                    cue.text
                ],
            )
            .map_err(|_| "The saved captions could not be updated.".to_string())?;
    }
    transaction
        .commit()
        .map_err(|_| "The clip draft could not be saved.".to_string())?;
    read_clip_draft(connection, source_path, identity)?
        .ok_or_else(|| "The clip draft could not be found.".to_string())
}

pub(crate) fn save_clip_draft(
    app: &AppHandle,
    source_path: &str,
    draft: &StoredClipDraft,
) -> Result<StoredClipDraft, String> {
    let source = validated_source(source_path)?;
    let media = probe_media(source.clone())?;
    let identity = source_identity(&source)?;
    if identity.size_bytes as u64 != media.file_size_bytes {
        return Err("The recording changed while saving this clip draft.".to_string());
    }
    let mut connection = open_database(app)?;
    persist_clip_draft(
        &mut connection,
        &source,
        &identity,
        media.duration_seconds,
        draft,
    )
}

fn mark_reviewed(
    connection: &Connection,
    source_path: &str,
    identity: &SourceIdentity,
) -> Result<StoredClipDraft, String> {
    ensure_source_identity(connection, source_path)?;
    let draft = read_clip_draft(connection, source_path, identity)?
        .ok_or_else(|| "Save a clip draft before reviewing it.".to_string())?;
    if draft.captions.is_empty() {
        return Err("Add captions before reviewing the clip.".to_string());
    }
    connection
        .execute(
            "UPDATE clip_draft SET reviewed = 1, review_session = ?2 WHERE source_path = ?1",
            params![source_path, review_session()],
        )
        .map_err(|_| "The clip review could not be saved.".to_string())?;
    read_clip_draft(connection, source_path, identity)?
        .ok_or_else(|| "The clip draft could not be found.".to_string())
}

pub(crate) fn mark_clip_reviewed(
    app: &AppHandle,
    source_path: &str,
) -> Result<StoredClipDraft, String> {
    let source = validated_source(source_path)?;
    let identity = source_identity(&source)?;
    let connection = open_database(app)?;
    mark_reviewed(&connection, &source, &identity)
}

fn checked_export_path(source_path: &str, export_path: &Path) -> Result<PathBuf, String> {
    let export = export_path
        .canonicalize()
        .map_err(|_| "The completed export could not be found.".to_string())?;
    if !export.is_file()
        || export == Path::new(source_path)
        || !export
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
        || fs::metadata(&export).map_or(true, |metadata| metadata.len() == 0)
    {
        return Err("The completed export is not a valid MP4 file.".to_string());
    }
    Ok(export)
}

fn export_sha256(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|_| "The completed export could not be read.".to_string())?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|_| "The completed export could not be read.".to_string())?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn persist_completed_export(
    connection: &Connection,
    source: &str,
    identity: &SourceIdentity,
    export: &Path,
) -> Result<String, String> {
    ensure_source_identity(connection, source)?;
    let hash = export_sha256(export)?;
    connection
        .execute(
            "INSERT INTO completed_export
             (source_path, source_size_bytes, source_modified_ns, export_path, export_sha256)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(source_path) DO UPDATE SET
              source_size_bytes = excluded.source_size_bytes,
              source_modified_ns = excluded.source_modified_ns,
              export_path = excluded.export_path,
              export_sha256 = excluded.export_sha256",
            params![
                source,
                identity.size_bytes,
                identity.modified_ns,
                export.to_string_lossy().as_ref(),
                hash
            ],
        )
        .map_err(|_| "The completed export reference could not be saved.".to_string())?;
    Ok(hash)
}

pub(crate) fn save_completed_export(
    app: &AppHandle,
    source_path: &str,
    export_path: &Path,
) -> Result<(), String> {
    let source = validated_source(source_path)?;
    let identity = source_identity(&source)?;
    let export = checked_export_path(&source, export_path)?;
    let connection = open_database(app)?;
    persist_completed_export(&connection, &source, &identity, &export)?;
    Ok(())
}

fn read_completed_export(
    connection: &Connection,
    source: &str,
    identity: &SourceIdentity,
) -> Result<Option<(PathBuf, String)>, String> {
    ensure_source_identity(connection, source)?;
    let saved: Option<(i64, String, String, String)> = connection
        .query_row(
            "SELECT source_size_bytes, source_modified_ns, export_path, export_sha256
             FROM completed_export WHERE source_path = ?1",
            params![source],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()
        .map_err(|_| "The completed export reference could not be read.".to_string())?;
    let Some((size, modified, path, expected_hash)) = saved else {
        return Ok(None);
    };
    if size != identity.size_bytes || modified != identity.modified_ns {
        return Err("The recording changed since the export was created.".to_string());
    }
    let Ok(export) = checked_export_path(source, Path::new(&path)) else {
        return Ok(None);
    };
    let actual_hash = export_sha256(&export)?;
    if actual_hash != expected_hash {
        return Err("The completed export changed since it was created. Export the clip again before publishing.".to_string());
    }
    Ok(Some((export, actual_hash)))
}

pub(crate) fn load_completed_export_with_sha256(
    app: &AppHandle,
    source_path: &str,
) -> Result<Option<(PathBuf, String)>, String> {
    let source = validated_source(source_path)?;
    let identity = source_identity(&source)?;
    let connection = open_database(app)?;
    read_completed_export(&connection, &source, &identity)
}

pub(crate) fn load_completed_export(
    app: &AppHandle,
    source_path: &str,
) -> Result<Option<PathBuf>, String> {
    load_completed_export_with_sha256(app, source_path).map(|export| export.map(|(path, _)| path))
}

fn import_media_sync(app: AppHandle, path: String) -> Result<MediaInfo, String> {
    let info = probe_media(path)?;
    let connection = open_database(&app)?;
    app.asset_protocol_scope()
        .allow_file(&info.path)
        .map_err(|_| "The recording could not be opened for preview.".to_string())?;
    save_recent(&connection, &info.path).map_err(|_| {
        "The recording was read but could not be saved to the project database.".to_string()
    })?;
    Ok(info)
}

#[tauri::command]
pub async fn import_media(app: AppHandle, path: String) -> Result<MediaInfo, String> {
    tauri::async_runtime::spawn_blocking(move || import_media_sync(app, path))
        .await
        .map_err(|_| "The recording import task stopped unexpectedly.".to_string())?
}

pub(crate) fn load_recent_media(app: AppHandle) -> Result<Option<MediaInfo>, String> {
    let connection = open_database(&app)?;
    match recent_path(&connection)
        .map_err(|_| "The recent recording could not be read.".to_string())?
    {
        Some(path) => {
            let info = probe_media(path)?;
            app.asset_protocol_scope()
                .allow_file(&info.path)
                .map_err(|_| "The recording could not be opened for preview.".to_string())?;
            Ok(Some(info))
        }
        None => Ok(None),
    }
}

#[tauri::command]
pub async fn load_recent_media_command(app: AppHandle) -> Result<Option<MediaInfo>, String> {
    tauri::async_runtime::spawn_blocking(move || load_recent_media(app))
        .await
        .map_err(|_| "The recording restore task stopped unexpectedly.".to_string())?
}

#[cfg(test)]
mod tests {
    use super::{
        DiscoveryState, FramingAnalysisMode, ReviewDecision, StoredCaptionCue, StoredClipDraft,
        StoredFramingKeyframe, StoredFramingMode, TranscriptionState, append_discovery_result,
        checked_export_path, complete_chunk, create_discovery_job, create_job, edit_segment,
        ensure_source_identity, mark_reviewed, migrate, persist_clip_draft,
        persist_completed_export, prepare_database, read_clip_draft, read_completed_export,
        read_discovery_job, read_framing_analysis_mode, read_job, recent_path, review_suggestion,
        save_framing_analysis_mode, save_recent, source_identity, update_discovery_status,
    };
    use crate::discovery::ValidatedSuggestion;
    use crate::transcription::{TranscriptSegment, TranscriptionProvider, plan_chunks};
    use rusqlite::Connection;
    use std::fs;
    use std::fs::FileTimes;
    use std::time::{Duration, UNIX_EPOCH};

    #[test]
    fn migration_preserves_recent_import() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        save_recent(&connection, "C:/recordings/first.mp4").unwrap();
        migrate(&connection).unwrap();
        assert_eq!(
            recent_path(&connection).unwrap().as_deref(),
            Some("C:/recordings/first.mp4")
        );
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            11
        );
    }

    #[test]
    fn project_connections_enforce_foreign_keys() {
        let connection = Connection::open_in_memory().unwrap();
        prepare_database(&connection).unwrap();
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "foreign_keys", |row| row.get(0))
                .unwrap(),
            1
        );
        assert!(
            connection
                .execute(
                    "INSERT INTO transcription_chunk
             (source_path, chunk_index, start_ms, end_ms, core_start_ms, core_end_ms)
             VALUES ('missing.mp4', 0, 0, 1000, 0, 1000)",
                    [],
                )
                .is_err()
        );
    }

    #[test]
    fn only_latest_import_is_restored() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        assert_eq!(recent_path(&connection).unwrap(), None);
        save_recent(&connection, "C:/recordings/first.mp4").unwrap();
        save_recent(&connection, "C:/recordings/second.mp4").unwrap();
        assert_eq!(
            recent_path(&connection).unwrap().as_deref(),
            Some("C:/recordings/second.mp4")
        );
    }

    #[test]
    fn framing_analysis_preference_is_isolated_per_project() {
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        assert_eq!(
            read_framing_analysis_mode(&connection, "C:/first.mp4").unwrap(),
            FramingAnalysisMode::Local
        );
        save_framing_analysis_mode(&connection, "C:/first.mp4", FramingAnalysisMode::Gemini)
            .unwrap();
        assert_eq!(
            read_framing_analysis_mode(&connection, "C:/first.mp4").unwrap(),
            FramingAnalysisMode::Gemini
        );
        assert_eq!(
            read_framing_analysis_mode(&connection, "C:/second.mp4").unwrap(),
            FramingAnalysisMode::Local
        );
        migrate(&connection).unwrap();
        assert_eq!(
            read_framing_analysis_mode(&connection, "C:/first.mp4").unwrap(),
            FramingAnalysisMode::Gemini
        );
    }

    #[test]
    fn v1_migration_keeps_import_and_adds_transcription_tables() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE recent_import (id INTEGER PRIMARY KEY, source_path TEXT NOT NULL);
             INSERT INTO recent_import VALUES (1, 'C:/first.mp4');
             PRAGMA user_version = 1;",
            )
            .unwrap();
        migrate(&connection).unwrap();
        migrate(&connection).unwrap();
        assert_eq!(
            recent_path(&connection).unwrap().as_deref(),
            Some("C:/first.mp4")
        );
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
                .unwrap(),
            11
        );
    }

    fn word(start_seconds: f64, end_seconds: f64, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_seconds,
            end_seconds,
            text: text.to_string(),
            speaker: Some("Speaker 1".to_string()),
        }
    }

    #[test]
    fn chunk_completion_is_atomic_and_resume_keeps_corrections() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let chunks = plan_chunks(70_000, 30_000, 2_000).unwrap();
        let job = create_job(
            &mut connection,
            "C:/first.mp4",
            70.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        assert_eq!((job.completed_chunks, job.total_chunks), (0, 3));
        assert!(
            complete_chunk(
                &mut connection,
                "C:/first.mp4",
                0,
                &[word(1.0, 1.4, "valid"), word(31.0, 31.5, "outside")],
                TranscriptionProvider::Gemini,
            )
            .is_err()
        );
        assert_eq!(
            read_job(&connection, "C:/first.mp4")
                .unwrap()
                .unwrap()
                .completed_chunks,
            0
        );
        let job = complete_chunk(
            &mut connection,
            "C:/first.mp4",
            0,
            &[word(1.0, 1.4, "valid")],
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        assert_eq!(job.completed_chunks, 1);
        let id = job.segments[0].id;
        edit_segment(&connection, "C:/first.mp4", id, "corrected", 1.05, 1.45).unwrap();
        let job = create_job(
            &mut connection,
            "C:/first.mp4",
            70.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        assert_eq!(job.completed_chunks, 1);
        assert_eq!(job.segments[0].text, "corrected");
        assert!(job.segments[0].corrected);
        let job = complete_chunk(
            &mut connection,
            "C:/first.mp4",
            0,
            &[word(2.0, 2.4, "duplicate")],
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        assert_eq!(job.segments.len(), 1);
        assert_eq!(job.segments[0].text, "corrected");
    }

    #[test]
    fn transcript_phrases_keep_their_actual_provider_after_resuming_with_another() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let chunks = plan_chunks(20_000, 10_000, 0).unwrap();
        create_job(
            &mut connection,
            "C:/mixed.mp4",
            20.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        let job = complete_chunk(
            &mut connection,
            "C:/mixed.mp4",
            0,
            &[word(1.0, 1.3, "first")],
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        edit_segment(
            &connection,
            "C:/mixed.mp4",
            job.segments[0].id,
            "corrected",
            1.0,
            1.3,
        )
        .unwrap();
        create_job(
            &mut connection,
            "C:/mixed.mp4",
            20.0,
            &chunks,
            None,
            TranscriptionProvider::Elevenlabs,
        )
        .unwrap();
        let job = complete_chunk(
            &mut connection,
            "C:/mixed.mp4",
            1,
            &[word(11.0, 11.3, "second")],
            TranscriptionProvider::Elevenlabs,
        )
        .unwrap();
        assert_eq!(job.provider, TranscriptionProvider::Elevenlabs);
        assert_eq!(job.segments.len(), 2);
        assert_eq!(job.segments[0].text, "corrected");
        assert!(job.segments[0].corrected);
        assert_eq!(
            job.segments[0].provider,
            Some(TranscriptionProvider::Gemini)
        );
        assert_eq!(
            job.segments[1].provider,
            Some(TranscriptionProvider::Elevenlabs)
        );
    }

    #[test]
    fn local_resume_keeps_cloud_chunks_and_corrections() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let chunks = plan_chunks(20_000, 10_000, 0).unwrap();
        create_job(
            &mut connection,
            "C:/cloud-local.mp4",
            20.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        let first = complete_chunk(
            &mut connection,
            "C:/cloud-local.mp4",
            0,
            &[word(1.0, 1.4, "cloud text")],
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        edit_segment(
            &connection,
            "C:/cloud-local.mp4",
            first.segments[0].id,
            "reviewed text",
            1.0,
            1.4,
        )
        .unwrap();
        let resumed = create_job(
            &mut connection,
            "C:/cloud-local.mp4",
            20.0,
            &chunks,
            None,
            TranscriptionProvider::Local,
        )
        .unwrap();
        assert_eq!(resumed.completed_chunks, 1);
        assert_eq!(resumed.segments[0].id, first.segments[0].id);
        let finished = complete_chunk(
            &mut connection,
            "C:/cloud-local.mp4",
            1,
            &[word(11.0, 11.4, "local text")],
            TranscriptionProvider::Local,
        )
        .unwrap();
        assert_eq!(finished.provider, TranscriptionProvider::Local);
        assert_eq!(finished.segments[0].text, "reviewed text");
        assert!(finished.segments[0].corrected);
        assert_eq!(
            finished.segments[0].provider,
            Some(TranscriptionProvider::Gemini)
        );
        assert_eq!(
            finished.segments[1].provider,
            Some(TranscriptionProvider::Local)
        );
    }

    #[test]
    fn jobs_for_different_imports_are_independent() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let chunks = plan_chunks(10_000, 10_000, 0).unwrap();
        create_job(
            &mut connection,
            "C:/first.mp4",
            10.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        complete_chunk(
            &mut connection,
            "C:/first.mp4",
            0,
            &[word(1.0, 1.3, "one")],
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        save_recent(&connection, "C:/second.mp4").unwrap();
        create_job(
            &mut connection,
            "C:/second.mp4",
            10.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        assert_eq!(
            read_job(&connection, "C:/first.mp4")
                .unwrap()
                .unwrap()
                .segments[0]
                .text,
            "one"
        );
        assert_eq!(
            read_job(&connection, "C:/second.mp4")
                .unwrap()
                .unwrap()
                .segments
                .len(),
            0
        );
        assert_eq!(
            recent_path(&connection).unwrap().as_deref(),
            Some("C:/second.mp4")
        );
    }

    #[test]
    fn correction_rejects_out_of_duration_and_keeps_original() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let chunks = plan_chunks(10_000, 10_000, 0).unwrap();
        create_job(
            &mut connection,
            "C:/first.mp4",
            10.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        let job = complete_chunk(
            &mut connection,
            "C:/first.mp4",
            0,
            &[word(1.0, 1.3, "original")],
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        let id = job.segments[0].id;
        assert!(edit_segment(&connection, "C:/first.mp4", id, "invalid", 9.0, 11.0).is_err());
        let job = read_job(&connection, "C:/first.mp4").unwrap().unwrap();
        assert_eq!(job.segments[0].text, "original");
        assert!(!job.segments[0].corrected);
        assert_eq!(job.state, TranscriptionState::Completed);
    }

    fn suggestion(start_seconds: f64, end_seconds: f64) -> ValidatedSuggestion {
        ValidatedSuggestion {
            start_seconds,
            end_seconds,
            hook: "Clear hook".into(),
            rationale: "Self-contained moment".into(),
            source_quote: "original words".into(),
            score: Some(0.8),
        }
    }

    fn completed_transcript(connection: &mut Connection, source: &str) {
        let chunks = plan_chunks(60_000, 60_000, 0).unwrap();
        create_job(
            connection,
            source,
            60.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        complete_chunk(
            connection,
            source,
            0,
            &[word(10.0, 11.0, "original words")],
            TranscriptionProvider::Gemini,
        )
        .unwrap();
    }

    #[test]
    fn v2_migration_preserves_completed_transcript_and_corrections() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        completed_transcript(&mut connection, "C:/first.mp4");
        let segment_id = read_job(&connection, "C:/first.mp4")
            .unwrap()
            .unwrap()
            .segments[0]
            .id;
        edit_segment(
            &connection,
            "C:/first.mp4",
            segment_id,
            "corrected words",
            10.1,
            11.1,
        )
        .unwrap();
        connection.pragma_update(None, "user_version", 2).unwrap();
        connection
            .execute_batch(
                "DROP TABLE clip_caption; DROP TABLE clip_draft; DROP TABLE completed_export;
             DROP TABLE discovery_suggestion; DROP TABLE discovery_window; DROP TABLE discovery_job;
             ALTER TABLE transcription_job DROP COLUMN source_size_bytes;
             ALTER TABLE transcription_job DROP COLUMN source_modified_ns;
             ALTER TABLE transcription_job DROP COLUMN provider;
             ALTER TABLE transcription_chunk DROP COLUMN provider;",
            )
            .unwrap();
        migrate(&connection).unwrap();
        migrate(&connection).unwrap();
        let restored = read_job(&connection, "C:/first.mp4").unwrap().unwrap();
        assert_eq!(restored.segments[0].text, "corrected words");
        assert!(restored.segments[0].corrected);
        assert_eq!(restored.completed_chunks, 1);
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            11
        );
    }

    #[test]
    fn discovery_resume_is_atomic_and_preserves_reviewed_suggestions() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        completed_transcript(&mut connection, "C:/first.mp4");
        let revision = "a".repeat(64);
        let job = create_discovery_job(
            &mut connection,
            "C:/first.mp4",
            "claude",
            "model",
            &revision,
            2,
        )
        .unwrap();
        assert_eq!((job.completed_windows, job.total_windows), (0, 2));
        assert!(
            append_discovery_result(
                &mut connection,
                "C:/first.mp4",
                0,
                &[suggestion(10.0, 12.0), suggestion(59.0, 61.0)]
            )
            .is_err()
        );
        assert_eq!(
            read_discovery_job(&connection, "C:/first.mp4")
                .unwrap()
                .unwrap()
                .completed_windows,
            0
        );
        let job = append_discovery_result(
            &mut connection,
            "C:/first.mp4",
            0,
            &[suggestion(10.0, 12.0)],
        )
        .unwrap();
        assert_eq!(job.completed_windows, 1);
        let id = job.suggestions[0].id;
        let reviewed = review_suggestion(
            &connection,
            "C:/first.mp4",
            id,
            ReviewDecision::Accepted,
            9.5,
            12.5,
        )
        .unwrap();
        assert_eq!(
            reviewed.suggestions[0].review_decision,
            ReviewDecision::Accepted
        );
        update_discovery_status(
            &connection,
            "C:/first.mp4",
            DiscoveryState::Failed,
            "Interrupted",
            Some("Provider timeout"),
        )
        .unwrap();
        let resumed = create_discovery_job(
            &mut connection,
            "C:/first.mp4",
            "claude",
            "model",
            &revision,
            2,
        )
        .unwrap();
        assert_eq!(resumed.completed_windows, 1);
        assert_eq!(resumed.suggestions[0].start_seconds, 9.5);
        assert_eq!(
            resumed.suggestions[0].review_decision,
            ReviewDecision::Accepted
        );
        let completed = append_discovery_result(
            &mut connection,
            "C:/first.mp4",
            1,
            &[suggestion(20.0, 22.0)],
        )
        .unwrap();
        assert_eq!(completed.state, DiscoveryState::Completed);
        assert_eq!(completed.suggestions.len(), 2);
    }

    #[test]
    fn discovery_keeps_distinct_overlapping_cuts_but_removes_near_duplicates() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        completed_transcript(&mut connection, "C:/first.mp4");
        create_discovery_job(
            &mut connection,
            "C:/first.mp4",
            "deepseek",
            "deepseek-flash",
            &"a".repeat(64),
            1,
        )
        .unwrap();
        let job = append_discovery_result(
            &mut connection,
            "C:/first.mp4",
            0,
            &[
                suggestion(10.0, 30.0),
                suggestion(20.0, 40.0),
                suggestion(10.5, 30.5),
            ],
        )
        .unwrap();
        assert_eq!(job.suggestions.len(), 2);
        assert_eq!(job.suggestions[0].original_start_seconds, 10.0);
        assert_eq!(job.suggestions[1].original_start_seconds, 20.0);
    }

    #[test]
    fn revised_transcript_archives_review_and_rejects_stale_ids() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        completed_transcript(&mut connection, "C:/first.mp4");
        let first = "a".repeat(64);
        let second = "b".repeat(64);
        create_discovery_job(
            &mut connection,
            "C:/first.mp4",
            "openai",
            "model",
            &first,
            1,
        )
        .unwrap();
        let old = append_discovery_result(
            &mut connection,
            "C:/first.mp4",
            0,
            &[suggestion(10.0, 12.0)],
        )
        .unwrap();
        let old_id = old.suggestions[0].id;
        review_suggestion(
            &connection,
            "C:/first.mp4",
            old_id,
            ReviewDecision::Rejected,
            10.0,
            12.0,
        )
        .unwrap();
        let current = create_discovery_job(
            &mut connection,
            "C:/first.mp4",
            "openai",
            "model",
            &second,
            1,
        )
        .unwrap();
        assert!(current.suggestions.is_empty());
        assert_eq!(current.transcript_revision, second);
        assert!(
            review_suggestion(
                &connection,
                "C:/first.mp4",
                old_id,
                ReviewDecision::Accepted,
                10.0,
                12.0
            )
            .is_err()
        );
        let old_decision: String = connection
            .query_row(
                "SELECT review_decision FROM discovery_suggestion WHERE id = ?1",
                [old_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_decision, "rejected");
    }

    #[test]
    fn review_validates_bounds_and_source() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        completed_transcript(&mut connection, "C:/first.mp4");
        completed_transcript(&mut connection, "C:/second.mp4");
        create_discovery_job(
            &mut connection,
            "C:/first.mp4",
            "claude",
            "model",
            &"a".repeat(64),
            1,
        )
        .unwrap();
        create_discovery_job(
            &mut connection,
            "C:/second.mp4",
            "claude",
            "model",
            &"b".repeat(64),
            1,
        )
        .unwrap();
        let first = append_discovery_result(
            &mut connection,
            "C:/first.mp4",
            0,
            &[suggestion(10.0, 12.0)],
        )
        .unwrap();
        let id = first.suggestions[0].id;
        assert!(
            review_suggestion(
                &connection,
                "C:/first.mp4",
                id,
                ReviewDecision::Accepted,
                59.0,
                61.0
            )
            .is_err()
        );
        assert!(
            review_suggestion(
                &connection,
                "C:/second.mp4",
                id,
                ReviewDecision::Accepted,
                10.0,
                12.0
            )
            .is_err()
        );
        assert_eq!(
            read_discovery_job(&connection, "C:/first.mp4")
                .unwrap()
                .unwrap()
                .suggestions[0]
                .review_decision,
            ReviewDecision::Pending
        );
    }

    #[test]
    fn changed_source_same_path_and_size_cannot_resume_old_transcript() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("episode.wav");
        fs::write(&path, b"original").unwrap();
        let source = path.to_string_lossy().into_owned();
        let identity = source_identity(&source).unwrap();
        let chunks = plan_chunks(10_000, 10_000, 0).unwrap();
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        create_job(
            &mut connection,
            &source,
            10.0,
            &chunks,
            Some(&identity),
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        ensure_source_identity(&connection, &source).unwrap();

        fs::write(&path, b"replaced").unwrap();
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(
            FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        )
        .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 8);
        assert!(ensure_source_identity(&connection, &source).is_err());
        assert!(
            create_job(
                &mut connection,
                &source,
                10.0,
                &chunks,
                Some(&source_identity(&source).unwrap()),
                TranscriptionProvider::Gemini
            )
            .is_err()
        );
        assert!(read_job(&connection, &source).unwrap().is_some());
    }

    #[test]
    fn migrated_legacy_transcript_requires_reimport_copy_but_preserves_rows() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("episode.wav");
        fs::write(&path, b"recording").unwrap();
        let source = path.to_string_lossy().into_owned();
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let chunks = plan_chunks(10_000, 10_000, 0).unwrap();
        create_job(
            &mut connection,
            &source,
            10.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        complete_chunk(
            &mut connection,
            &source,
            0,
            &[word(1.0, 1.3, "saved")],
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        connection
            .execute_batch(
                "DROP TABLE clip_caption; DROP TABLE clip_draft; DROP TABLE completed_export;
              ALTER TABLE transcription_job DROP COLUMN source_size_bytes;
             ALTER TABLE transcription_job DROP COLUMN source_modified_ns;
             ALTER TABLE transcription_job DROP COLUMN provider;
             ALTER TABLE transcription_chunk DROP COLUMN provider;
             PRAGMA user_version = 3;",
            )
            .unwrap();
        migrate(&connection).unwrap();
        assert!(ensure_source_identity(&connection, &source).is_err());
        assert_eq!(
            read_job(&connection, &source).unwrap().unwrap().segments[0].text,
            "saved"
        );
    }

    fn example_draft() -> StoredClipDraft {
        StoredClipDraft {
            start_ms: 10_000,
            end_ms: 20_000,
            framing_mode: StoredFramingMode::Crop,
            horizontal_percent: 50,
            framing_keyframes: vec![],
            captions: vec![StoredCaptionCue {
                start_ms: 10_000,
                end_ms: 12_000,
                text: "Saved caption".into(),
            }],
            reviewed: true,
        }
    }

    #[test]
    fn clip_draft_edits_reset_review_and_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("episode.wav");
        fs::write(&source_path, b"recording").unwrap();
        let source = source_path.to_string_lossy().into_owned();
        let identity = source_identity(&source).unwrap();
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let mut draft = example_draft();
        draft.framing_keyframes = vec![
            StoredFramingKeyframe {
                timestamp_ms: 10_000,
                mode: StoredFramingMode::Crop,
                horizontal_percent: 25,
            },
            StoredFramingKeyframe {
                timestamp_ms: 15_000,
                mode: StoredFramingMode::Fit,
                horizontal_percent: 50,
            },
        ];
        let saved = persist_clip_draft(&mut connection, &source, &identity, 60.0, &draft).unwrap();
        assert!(!saved.reviewed);
        assert!(
            mark_reviewed(&connection, &source, &identity)
                .unwrap()
                .reviewed
        );
        draft.captions[0].text = "Corrected caption".into();
        let updated =
            persist_clip_draft(&mut connection, &source, &identity, 60.0, &draft).unwrap();
        assert_eq!(updated.captions[0].text, "Corrected caption");
        assert!(!updated.reviewed);
        mark_reviewed(&connection, &source, &identity).unwrap();
        connection
            .execute(
                "UPDATE clip_draft SET review_session = 'previous-process' WHERE source_path = ?1",
                [&source],
            )
            .unwrap();
        let restored = read_clip_draft(&connection, &source, &identity)
            .unwrap()
            .unwrap();
        assert_eq!(restored.start_ms, 10_000);
        assert_eq!(restored.captions[0].text, "Corrected caption");
        assert_eq!(restored.framing_keyframes, draft.framing_keyframes);
        assert!(!restored.reviewed);
    }

    #[test]
    fn v9_draft_migration_preserves_legacy_framing_and_captions() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("episode.wav");
        fs::write(&source_path, b"recording").unwrap();
        let source = source_path.to_string_lossy().into_owned();
        let identity = source_identity(&source).unwrap();
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        persist_clip_draft(&mut connection, &source, &identity, 60.0, &example_draft()).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE clip_draft DROP COLUMN framing_keyframes;
             PRAGMA user_version = 9;",
            )
            .unwrap();
        migrate(&connection).unwrap();
        let restored = read_clip_draft(&connection, &source, &identity)
            .unwrap()
            .unwrap();
        assert_eq!(restored.framing_mode, StoredFramingMode::Crop);
        assert_eq!(restored.horizontal_percent, 50);
        assert!(restored.framing_keyframes.is_empty());
        assert_eq!(restored.captions[0].text, "Saved caption");
    }

    #[test]
    fn invalid_draft_does_not_replace_saved_captions_and_stale_source_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("episode.wav");
        fs::write(&source_path, b"recording").unwrap();
        let source = source_path.to_string_lossy().into_owned();
        let identity = source_identity(&source).unwrap();
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        persist_clip_draft(&mut connection, &source, &identity, 60.0, &example_draft()).unwrap();
        let mut bad = example_draft();
        bad.captions[0].end_ms = 70_000;
        assert!(persist_clip_draft(&mut connection, &source, &identity, 60.0, &bad).is_err());
        let mut bad_frames = example_draft();
        bad_frames.framing_keyframes = vec![StoredFramingKeyframe {
            timestamp_ms: 11_000,
            mode: StoredFramingMode::Crop,
            horizontal_percent: 50,
        }];
        assert!(
            persist_clip_draft(&mut connection, &source, &identity, 60.0, &bad_frames).is_err()
        );
        assert_eq!(
            read_clip_draft(&connection, &source, &identity)
                .unwrap()
                .unwrap()
                .captions[0]
                .text,
            "Saved caption"
        );
        fs::write(&source_path, b"replacement recording").unwrap();
        assert!(read_clip_draft(&connection, &source, &source_identity(&source).unwrap()).is_err());
        assert!(
            persist_clip_draft(
                &mut connection,
                &source,
                &source_identity(&source).unwrap(),
                60.0,
                &example_draft()
            )
            .is_err()
        );
    }

    #[test]
    fn completed_export_reference_checks_file_and_integrity() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("episode.wav");
        let export_path = directory.path().join("clip.mp4");
        fs::write(&source_path, b"recording").unwrap();
        fs::write(&export_path, b"rendered clip").unwrap();
        let source = source_path.to_string_lossy().into_owned();
        let identity = source_identity(&source).unwrap();
        let connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        assert!(checked_export_path(&source, &source_path).is_err());
        assert!(checked_export_path(&source, directory.path()).is_err());
        let export = checked_export_path(&source, &export_path).unwrap();
        let expected = persist_completed_export(&connection, &source, &identity, &export).unwrap();
        assert_eq!(
            read_completed_export(&connection, &source, &identity)
                .unwrap()
                .unwrap()
                .1,
            expected
        );
        fs::write(&export_path, b"tampered clip").unwrap();
        assert!(read_completed_export(&connection, &source, &identity).is_err());
        fs::remove_file(&export_path).unwrap();
        assert!(
            read_completed_export(&connection, &source, &identity)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn v4_to_v5_migration_keeps_existing_transcript_and_suggestions() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        completed_transcript(&mut connection, "C:/first.mp4");
        create_discovery_job(
            &mut connection,
            "C:/first.mp4",
            "claude",
            "model",
            &"a".repeat(64),
            1,
        )
        .unwrap();
        append_discovery_result(
            &mut connection,
            "C:/first.mp4",
            0,
            &[suggestion(10.0, 12.0)],
        )
        .unwrap();
        connection
            .execute_batch(
                "DROP TABLE clip_caption; DROP TABLE clip_draft; DROP TABLE completed_export;
             ALTER TABLE transcription_job DROP COLUMN provider;
             ALTER TABLE transcription_chunk DROP COLUMN provider;
             PRAGMA user_version = 4;",
            )
            .unwrap();
        migrate(&connection).unwrap();
        migrate(&connection).unwrap();
        assert_eq!(
            read_job(&connection, "C:/first.mp4")
                .unwrap()
                .unwrap()
                .segments
                .len(),
            1
        );
        assert_eq!(
            read_discovery_job(&connection, "C:/first.mp4")
                .unwrap()
                .unwrap()
                .suggestions
                .len(),
            1
        );
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            11
        );
    }

    #[test]
    fn v5_to_v6_migration_preserves_reviewed_suggestions_and_foreign_keys() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        completed_transcript(&mut connection, "C:/first.mp4");
        create_discovery_job(
            &mut connection,
            "C:/first.mp4",
            "claude",
            "model",
            &"a".repeat(64),
            1,
        )
        .unwrap();
        let job = append_discovery_result(
            &mut connection,
            "C:/first.mp4",
            0,
            &[suggestion(10.0, 12.0)],
        )
        .unwrap();
        let id = job.suggestions[0].id;
        review_suggestion(
            &connection,
            "C:/first.mp4",
            id,
            ReviewDecision::Accepted,
            9.5,
            12.5,
        )
        .unwrap();

        // Recreate the former v5 provider constraint while keeping its child rows.
        connection.pragma_update(None, "foreign_keys", 0).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE discovery_job_v5 (
                    source_path TEXT NOT NULL,
                    generation INTEGER NOT NULL CHECK (generation > 0),
                    provider TEXT NOT NULL CHECK (provider IN ('claude', 'openai')),
                    model TEXT NOT NULL,
                    transcript_revision TEXT NOT NULL,
                    state TEXT NOT NULL CHECK (state IN ('idle', 'running', 'completed', 'cancelled', 'failed')),
                    stage TEXT NOT NULL,
                    error TEXT,
                    total_windows INTEGER NOT NULL CHECK (total_windows > 0),
                    PRIMARY KEY (source_path, generation),
                    FOREIGN KEY (source_path) REFERENCES transcription_job(source_path)
                );
                INSERT INTO discovery_job_v5 SELECT * FROM discovery_job;
                DROP TABLE discovery_job;
                ALTER TABLE discovery_job_v5 RENAME TO discovery_job;
                ALTER TABLE clip_draft DROP COLUMN framing_keyframes;
                ALTER TABLE transcription_job DROP COLUMN provider;
                ALTER TABLE transcription_chunk DROP COLUMN provider;
                PRAGMA user_version = 5;
                PRAGMA foreign_keys = ON;",
            )
            .unwrap();
        assert!(connection.execute(
            "INSERT INTO discovery_job
             (source_path, generation, provider, model, transcript_revision, state, stage, total_windows)
             VALUES ('C:/first.mp4', 2, 'deepseek', 'deepseek-flash', 'b', 'idle', 'Starting', 1)",
            [],
        ).is_err());
        migrate(&connection).unwrap();
        migrate(&connection).unwrap();
        let restored = read_discovery_job(&connection, "C:/first.mp4")
            .unwrap()
            .unwrap();
        assert_eq!(restored.provider, "claude");
        assert_eq!(restored.suggestions[0].id, id);
        assert_eq!(restored.suggestions[0].start_seconds, 9.5);
        assert_eq!(
            restored.suggestions[0].review_decision,
            ReviewDecision::Accepted
        );
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "foreign_keys", |row| row.get(0))
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            11
        );
        create_discovery_job(
            &mut connection,
            "C:/first.mp4",
            "deepseek",
            "deepseek-flash",
            &"b".repeat(64),
            1,
        )
        .unwrap();
        assert_eq!(
            read_discovery_job(&connection, "C:/first.mp4")
                .unwrap()
                .unwrap()
                .provider,
            "deepseek"
        );
    }

    #[test]
    fn v6_to_v8_migration_preserves_transcript_corrections_and_provenance() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        completed_transcript(&mut connection, "C:/first.mp4");
        let id = read_job(&connection, "C:/first.mp4")
            .unwrap()
            .unwrap()
            .segments[0]
            .id;
        edit_segment(&connection, "C:/first.mp4", id, "corrected", 10.1, 11.1).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE clip_draft DROP COLUMN framing_keyframes;
                 ALTER TABLE transcription_job DROP COLUMN provider;
                 ALTER TABLE transcription_chunk DROP COLUMN provider;
                 PRAGMA user_version = 6;",
            )
            .unwrap();

        migrate(&connection).unwrap();
        migrate(&connection).unwrap();
        let job = read_job(&connection, "C:/first.mp4").unwrap().unwrap();
        assert_eq!(job.provider, TranscriptionProvider::Gemini);
        assert_eq!(job.completed_chunks, 1);
        assert_eq!(job.chunks[0].provider, Some(TranscriptionProvider::Gemini));
        assert_eq!(job.segments[0].text, "corrected");
        assert!(job.segments[0].corrected);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
    }

    #[test]
    fn provider_switch_preserves_completed_chunks_and_edits() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let chunks = plan_chunks(20_000, 10_000, 0).unwrap();
        create_job(
            &mut connection,
            "C:/first.mp4",
            20.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        let completed = complete_chunk(
            &mut connection,
            "C:/first.mp4",
            0,
            &[word(1.0, 1.3, "original")],
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        edit_segment(
            &connection,
            "C:/first.mp4",
            completed.segments[0].id,
            "corrected",
            1.0,
            1.3,
        )
        .unwrap();
        let switched = create_job(
            &mut connection,
            "C:/first.mp4",
            20.0,
            &chunks,
            None,
            TranscriptionProvider::Elevenlabs,
        )
        .unwrap();
        assert_eq!(switched.provider, TranscriptionProvider::Elevenlabs);
        assert_eq!(switched.completed_chunks, 1);
        assert_eq!(
            switched.chunks[0].provider,
            Some(TranscriptionProvider::Gemini)
        );
        assert_eq!(switched.chunks[1].provider, None);
        assert_eq!(switched.segments[0].text, "corrected");
        assert!(
            complete_chunk(
                &mut connection,
                "C:/first.mp4",
                1,
                &[word(11.0, 11.3, "second")],
                TranscriptionProvider::Gemini,
            )
            .is_err()
        );
        let finished = complete_chunk(
            &mut connection,
            "C:/first.mp4",
            1,
            &[word(11.0, 11.3, "second")],
            TranscriptionProvider::Elevenlabs,
        )
        .unwrap();
        assert_eq!(
            finished.chunks[0].provider,
            Some(TranscriptionProvider::Gemini)
        );
        assert_eq!(
            finished.chunks[1].provider,
            Some(TranscriptionProvider::Elevenlabs)
        );
        let err = create_job(
            &mut connection,
            "C:/first.mp4",
            20.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap_err();
        assert!(err.contains("already transcribed"));
        let saved = read_job(&connection, "C:/first.mp4").unwrap().unwrap();
        assert_eq!(saved.provider, TranscriptionProvider::Elevenlabs);
        assert_eq!(saved.segments[0].text, "corrected");
        assert!(saved.segments[0].corrected);
    }

    #[test]
    fn provider_can_change_before_any_chunk_completes() {
        let mut connection = Connection::open_in_memory().unwrap();
        migrate(&connection).unwrap();
        let chunks = plan_chunks(10_000, 10_000, 0).unwrap();
        create_job(
            &mut connection,
            "C:/first.mp4",
            10.0,
            &chunks,
            None,
            TranscriptionProvider::Gemini,
        )
        .unwrap();
        let job = create_job(
            &mut connection,
            "C:/first.mp4",
            10.0,
            &chunks,
            None,
            TranscriptionProvider::Elevenlabs,
        )
        .unwrap();
        assert_eq!(job.provider, TranscriptionProvider::Elevenlabs);
        assert_eq!(job.chunks[0].provider, None);
    }

    #[test]
    fn v10_to_v11_keeps_corrections_mixed_providers_and_foreign_keys() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE transcription_job (
                    source_path TEXT PRIMARY KEY,
                    duration_seconds REAL NOT NULL CHECK (duration_seconds > 0),
                    state TEXT NOT NULL CHECK (state IN ('idle', 'running', 'completed', 'cancelled', 'failed')),
                    stage TEXT NOT NULL, error TEXT, source_size_bytes INTEGER,
                    source_modified_ns TEXT, provider TEXT NOT NULL DEFAULT 'gemini'
                        CHECK (provider IN ('gemini', 'elevenlabs'))
                 );
                 CREATE TABLE transcription_chunk (
                    source_path TEXT NOT NULL REFERENCES transcription_job(source_path),
                    chunk_index INTEGER NOT NULL CHECK (chunk_index >= 0),
                    start_ms INTEGER NOT NULL, end_ms INTEGER NOT NULL,
                    core_start_ms INTEGER NOT NULL, core_end_ms INTEGER NOT NULL,
                    completed INTEGER NOT NULL DEFAULT 0 CHECK (completed IN (0, 1)),
                    provider TEXT CHECK (provider IN ('gemini', 'elevenlabs')),
                    PRIMARY KEY (source_path, chunk_index),
                    CHECK (start_ms < end_ms AND start_ms <= core_start_ms AND
                           core_start_ms < core_end_ms AND core_end_ms <= end_ms)
                 );
                 CREATE TABLE transcript_segment (
                    id INTEGER PRIMARY KEY, source_path TEXT NOT NULL,
                    chunk_index INTEGER NOT NULL, original_start_seconds REAL NOT NULL,
                    original_end_seconds REAL NOT NULL, original_text TEXT NOT NULL,
                    speaker TEXT, corrected_start_seconds REAL,
                    corrected_end_seconds REAL, corrected_text TEXT,
                    FOREIGN KEY (source_path, chunk_index)
                        REFERENCES transcription_chunk(source_path, chunk_index)
                 );
                 CREATE TABLE discovery_job (
                    source_path TEXT NOT NULL REFERENCES transcription_job(source_path),
                    generation INTEGER NOT NULL, PRIMARY KEY (source_path, generation)
                 );
                 INSERT INTO transcription_job VALUES
                    ('C:/episode.mp4', 40.0, 'completed', 'Done', NULL, 100, '1', 'elevenlabs');
                 INSERT INTO transcription_chunk VALUES
                    ('C:/episode.mp4', 0, 0, 21000, 0, 20000, 1, 'gemini'),
                    ('C:/episode.mp4', 1, 19000, 40000, 20000, 40000, 1, 'elevenlabs');
                 INSERT INTO transcript_segment VALUES
                    (42, 'C:/episode.mp4', 0, 1.0, 2.0, 'original', 'Speaker 1',
                     1.1, 2.1, 'corrected');
                 INSERT INTO discovery_job VALUES ('C:/episode.mp4', 1);
                 PRAGMA user_version = 10;",
            )
            .unwrap();

        migrate(&connection).unwrap();
        let job = read_job(&connection, "C:/episode.mp4").unwrap().unwrap();
        assert_eq!(job.segments[0].id, 42);
        assert_eq!(job.segments[0].text, "corrected");
        assert_eq!(
            job.segments[0].provider,
            Some(TranscriptionProvider::Gemini)
        );
        assert_eq!(
            job.chunks[1].provider,
            Some(TranscriptionProvider::Elevenlabs)
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        connection
            .execute("UPDATE transcription_job SET provider = 'local' WHERE source_path = 'C:/episode.mp4'", [])
            .unwrap();
        connection
            .execute("UPDATE transcription_chunk SET provider = 'local' WHERE source_path = 'C:/episode.mp4' AND chunk_index = 1", [])
            .unwrap();
    }

    #[test]
    fn migration_rejects_future_database_version() {
        let connection = Connection::open_in_memory().unwrap();
        connection.pragma_update(None, "user_version", 12).unwrap();
        assert!(migrate(&connection).is_err());
    }
}
