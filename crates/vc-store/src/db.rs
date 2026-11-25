//! SQLite metadata store.

use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};

use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use vc_core::{ConsentId, ErrorCode, JobId, JobState, Language, Rate, VoiceId};

const MIGRATIONS: &[&str] = &[
    include_str!("migrations/0001_init.sql"),
    include_str!("migrations/0002_job_rate.sql"),
    include_str!("migrations/0003_clean_voice.sql"),
];
/// `prev_hash` of the first consent record.
pub const GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("corrupt store: {0}")]
    Corrupt(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewConsent {
    pub speaker_name: String,
    pub relationship: String,
    pub attestation: String,
    pub languages: Vec<Language>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConsentRecord {
    pub id: ConsentId,
    pub seq: i64,
    pub speaker_name: String,
    pub relationship: String,
    pub attestation: String,
    pub languages: Vec<Language>,
    pub created_at_ms: i64,
    pub prev_hash: String,
    pub record_hash: String,
}

impl ConsentRecord {
    /// sha256 over the canonical JSON (sorted keys) of every field but `record_hash`.
    pub fn compute_hash(&self) -> String {
        let canonical = serde_json::json!({
            "attestation": self.attestation,
            "created_at_ms": self.created_at_ms,
            "id": self.id,
            "languages": self.languages,
            "prev_hash": self.prev_hash,
            "relationship": self.relationship,
            "seq": self.seq,
            "speaker_name": self.speaker_name,
        });
        hex::encode(Sha256::digest(canonical.to_string().as_bytes()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoiceRow {
    pub id: VoiceId,
    pub consent_id: ConsentId,
    pub name: String,
    pub reference_language: Language,
    pub ref_sample_rate: u32,
    pub ref_ms: u64,
    pub ref_sha256: String,
    pub created_at_ms: i64,
    /// The stored reference went through the enrollment cleanup.
    pub ref_cleaned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    pub id: JobId,
    pub voice_id: VoiceId,
    pub language: Language,
    pub text: String,
    pub text_sha256: String,
    pub chunks_total: usize,
    pub rate: Rate,
    /// Run the finishing polish on the joined output.
    pub polish: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    pub id: JobId,
    pub voice_id: VoiceId,
    pub state: JobState,
    pub language: Language,
    /// Present only while the job is queued or running.
    pub text: Option<String>,
    pub text_sha256: String,
    pub chunks_total: usize,
    pub chunks_done: usize,
    pub attempts: u32,
    pub error_code: Option<ErrorCode>,
    pub audio_ms: Option<u64>,
    pub output_sha256: Option<String>,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub first_audio_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub expires_at_ms: Option<i64>,
    pub rate: Rate,
    pub polish: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChunkRow {
    pub idx: usize,
    pub words: usize,
    pub chars: usize,
    pub audio_ms: u64,
    pub peak_rss_kib: u64,
    pub wall_ms: u64,
    pub out_sha256: String,
}

/// How a job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobEnd {
    Succeeded {
        audio_ms: u64,
        output_sha256: String,
    },
    Failed(ErrorCode),
    Cancelled,
}

#[derive(Debug)]
pub struct Store {
    conn: Mutex<Connection>,
}

fn parse<T: std::str::FromStr>(s: String, what: &str) -> rusqlite::Result<T> {
    s.parse().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("bad {what}: {s:?}").into(),
        )
    })
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let known = i64::try_from(MIGRATIONS.len()).unwrap_or(i64::MAX);
        if version > known {
            return Err(StoreError::Corrupt(format!(
                "schema version {version} is newer than this vcd ({known})"
            )));
        }
        for (i, sql) in MIGRATIONS
            .iter()
            .enumerate()
            .skip(usize::try_from(version).unwrap_or(0))
        {
            let tx = conn.unchecked_transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", i + 1)?;
            tx.commit()?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ---- consents -------------------------------------------------------------------

    pub fn insert_consent(&self, new: NewConsent, now_ms: i64) -> Result<ConsentRecord> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let last: Option<(i64, String)> = tx
            .query_row(
                "SELECT seq, record_hash FROM consents ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (seq, prev_hash) = match last {
            Some((seq, hash)) => (seq + 1, hash),
            None => (1, GENESIS_HASH.to_owned()),
        };
        let mut record = ConsentRecord {
            id: ConsentId::generate(),
            seq,
            speaker_name: new.speaker_name,
            relationship: new.relationship,
            attestation: new.attestation,
            languages: new.languages,
            created_at_ms: now_ms,
            prev_hash,
            record_hash: String::new(),
        };
        record.record_hash = record.compute_hash();
        let languages = serde_json::to_string(&record.languages)
            .map_err(|e| StoreError::Corrupt(e.to_string()))?;
        tx.execute(
            "INSERT INTO consents (id, seq, speaker_name, relationship, attestation, languages,
                                   created_at_ms, prev_hash, record_hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.id.to_string(),
                record.seq,
                record.speaker_name,
                record.relationship,
                record.attestation,
                languages,
                record.created_at_ms,
                record.prev_hash,
                record.record_hash,
            ],
        )?;
        tx.commit()?;
        Ok(record)
    }

    fn consent_from_row(r: &Row<'_>) -> rusqlite::Result<ConsentRecord> {
        let languages: String = r.get(5)?;
        Ok(ConsentRecord {
            id: parse(r.get(0)?, "consent id")?,
            seq: r.get(1)?,
            speaker_name: r.get(2)?,
            relationship: r.get(3)?,
            attestation: r.get(4)?,
            languages: serde_json::from_str(&languages).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into())
            })?,
            created_at_ms: r.get(6)?,
            prev_hash: r.get(7)?,
            record_hash: r.get(8)?,
        })
    }

    const CONSENT_COLS: &'static str = "id, seq, speaker_name, relationship, attestation, \
                                        languages, created_at_ms, prev_hash, record_hash";

    pub fn get_consent(&self, id: ConsentId) -> Result<ConsentRecord> {
        self.conn()
            .query_row(
                &format!("SELECT {} FROM consents WHERE id = ?1", Self::CONSENT_COLS),
                [id.to_string()],
                Self::consent_from_row,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// The latest record hash (the genesis hash when there are no records).
    pub fn consent_chain_head(&self) -> Result<String> {
        Ok(self
            .conn()
            .query_row(
                "SELECT record_hash FROM consents ORDER BY seq DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| GENESIS_HASH.to_owned()))
    }

    /// Recompute every record hash and link. Returns the chain head.
    pub fn verify_consent_chain(&self) -> Result<String> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM consents ORDER BY seq",
            Self::CONSENT_COLS
        ))?;
        let mut prev = GENESIS_HASH.to_owned();
        for (expected_seq, rec) in (1i64..).zip(stmt.query_map([], Self::consent_from_row)?) {
            let rec = rec?;
            if rec.seq != expected_seq
                || rec.prev_hash != prev
                || rec.compute_hash() != rec.record_hash
            {
                return Err(StoreError::Corrupt(format!(
                    "consent chain broken at seq {}",
                    rec.seq
                )));
            }
            prev = rec.record_hash;
        }
        Ok(prev)
    }

    // ---- voices ---------------------------------------------------------------------

    pub fn insert_voice(&self, v: &VoiceRow) -> Result<()> {
        self.conn().execute(
            "INSERT INTO voices (id, consent_id, name, reference_language, ref_sample_rate,
                                 ref_ms, ref_sha256, created_at_ms, ref_cleaned)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                v.id.to_string(),
                v.consent_id.to_string(),
                v.name,
                v.reference_language.code(),
                v.ref_sample_rate,
                to_i64(v.ref_ms),
                v.ref_sha256,
                v.created_at_ms,
                v.ref_cleaned,
            ],
        )?;
        Ok(())
    }

    fn voice_from_row(r: &Row<'_>) -> rusqlite::Result<VoiceRow> {
        Ok(VoiceRow {
            id: parse(r.get(0)?, "voice id")?,
            consent_id: parse(r.get(1)?, "consent id")?,
            name: r.get(2)?,
            reference_language: parse(r.get(3)?, "language")?,
            ref_sample_rate: r.get(4)?,
            ref_ms: to_u64(r.get(5)?),
            ref_sha256: r.get(6)?,
            created_at_ms: r.get(7)?,
            ref_cleaned: r.get(8)?,
        })
    }

    const VOICE_COLS: &'static str = "id, consent_id, name, reference_language, ref_sample_rate, \
                                      ref_ms, ref_sha256, created_at_ms, ref_cleaned";

    /// A live (not deleted) voice.
    pub fn get_voice(&self, id: VoiceId) -> Result<VoiceRow> {
        self.conn()
            .query_row(
                &format!(
                    "SELECT {} FROM voices WHERE id = ?1 AND deleted_at_ms IS NULL",
                    Self::VOICE_COLS
                ),
                [id.to_string()],
                Self::voice_from_row,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    pub fn list_voices(&self) -> Result<Vec<VoiceRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM voices WHERE deleted_at_ms IS NULL ORDER BY id",
            Self::VOICE_COLS
        ))?;
        let rows = stmt.query_map([], Self::voice_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Mark a voice deleted, unless a queued or running job still uses it.
    pub fn tombstone_voice(&self, id: VoiceId, now_ms: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM jobs WHERE voice_id = ?1 AND state IN ('queued', 'running')",
            [id.to_string()],
            |r| r.get(0),
        )?;
        if active > 0 {
            return Err(StoreError::Conflict("voice is in use by a job".into()));
        }
        let n = tx.execute(
            "UPDATE voices SET deleted_at_ms = ?2 WHERE id = ?1 AND deleted_at_ms IS NULL",
            params![id.to_string(), now_ms],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        tx.commit()?;
        Ok(())
    }

    // ---- jobs -----------------------------------------------------------------------

    /// Insert a queued job; fails with `NotFound` if the voice is gone.
    pub fn insert_job(&self, j: &NewJob, now_ms: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let live: i64 = tx.query_row(
            "SELECT COUNT(*) FROM voices WHERE id = ?1 AND deleted_at_ms IS NULL",
            [j.voice_id.to_string()],
            |r| r.get(0),
        )?;
        if live == 0 {
            return Err(StoreError::NotFound);
        }
        tx.execute(
            "INSERT INTO jobs (id, voice_id, state, language, text, text_sha256, chunks_total,
                               created_at_ms, rate_milli, polish)
             VALUES (?1, ?2, 'queued', ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                j.id.to_string(),
                j.voice_id.to_string(),
                j.language.code(),
                j.text,
                j.text_sha256,
                j.chunks_total,
                now_ms,
                j.rate.milli(),
                j.polish,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    const JOB_COLS: &'static str = "id, voice_id, state, language, text, text_sha256, \
        chunks_total, chunks_done, attempts, error_code, audio_ms, output_sha256, \
        created_at_ms, started_at_ms, first_audio_at_ms, finished_at_ms, expires_at_ms, rate_milli, \
        polish";

    fn job_from_row(r: &Row<'_>) -> rusqlite::Result<JobRow> {
        let error_code: Option<String> = r.get(9)?;
        Ok(JobRow {
            id: parse(r.get(0)?, "job id")?,
            voice_id: parse(r.get(1)?, "voice id")?,
            state: parse(r.get(2)?, "state")?,
            language: parse(r.get(3)?, "language")?,
            text: r.get(4)?,
            text_sha256: r.get(5)?,
            chunks_total: r.get(6)?,
            chunks_done: r.get(7)?,
            attempts: r.get(8)?,
            error_code: error_code.as_deref().and_then(ErrorCode::parse),
            audio_ms: r.get::<_, Option<i64>>(10)?.map(to_u64),
            output_sha256: r.get(11)?,
            created_at_ms: r.get(12)?,
            started_at_ms: r.get(13)?,
            first_audio_at_ms: r.get(14)?,
            finished_at_ms: r.get(15)?,
            expires_at_ms: r.get(16)?,
            rate: Rate::from_milli(r.get(17)?).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    17,
                    rusqlite::types::Type::Integer,
                    Box::new(e),
                )
            })?,
            polish: r.get(18)?,
        })
    }

    pub fn get_job(&self, id: JobId) -> Result<JobRow> {
        self.conn()
            .query_row(
                &format!("SELECT {} FROM jobs WHERE id = ?1", Self::JOB_COLS),
                [id.to_string()],
                Self::job_from_row,
            )
            .optional()?
            .ok_or(StoreError::NotFound)
    }

    /// Jobs in `state`, oldest first.
    pub fn jobs_in_state(&self, state: JobState) -> Result<Vec<JobRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM jobs WHERE state = ?1 ORDER BY id",
            Self::JOB_COLS
        ))?;
        let rows = stmt.query_map([state.as_str()], Self::job_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// queued -> running. Counts an attempt and resets per-attempt progress.
    pub fn start_job(&self, id: JobId, now_ms: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM chunks WHERE job_id = ?1", [id.to_string()])?;
        let n = tx.execute(
            "UPDATE jobs SET state = 'running', attempts = attempts + 1, chunks_done = 0,
                             started_at_ms = ?2, first_audio_at_ms = NULL
             WHERE id = ?1 AND state = 'queued'",
            params![id.to_string(), now_ms],
        )?;
        if n == 0 {
            return Err(StoreError::Conflict("job is not queued".into()));
        }
        tx.commit()?;
        Ok(())
    }

    /// Record one finished chunk of a running job.
    pub fn record_chunk(&self, id: JobId, c: &ChunkRow, now_ms: i64) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let n = tx.execute(
            "UPDATE jobs SET chunks_done = chunks_done + 1,
                             first_audio_at_ms = COALESCE(first_audio_at_ms, ?2)
             WHERE id = ?1 AND state = 'running' AND chunks_done = ?3",
            params![id.to_string(), now_ms, c.idx],
        )?;
        if n == 0 {
            return Err(StoreError::Conflict(
                "chunk out of order or job not running".into(),
            ));
        }
        tx.execute(
            "INSERT INTO chunks (job_id, idx, words, chars, audio_ms, peak_rss_kib, wall_ms,
                                 out_sha256)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id.to_string(),
                c.idx,
                c.words,
                c.chars,
                to_i64(c.audio_ms),
                to_i64(c.peak_rss_kib),
                to_i64(c.wall_ms),
                c.out_sha256,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn chunks(&self, id: JobId) -> Result<Vec<ChunkRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT idx, words, chars, audio_ms, peak_rss_kib, wall_ms, out_sha256
             FROM chunks WHERE job_id = ?1 ORDER BY idx",
        )?;
        let rows = stmt.query_map([id.to_string()], |r| {
            Ok(ChunkRow {
                idx: r.get(0)?,
                words: r.get(1)?,
                chars: r.get(2)?,
                audio_ms: to_u64(r.get(3)?),
                peak_rss_kib: to_u64(r.get(4)?),
                wall_ms: to_u64(r.get(5)?),
                out_sha256: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// End a job: running -> succeeded/failed/cancelled, or queued -> cancelled. Clears
    /// the job text and sets the expiry.
    pub fn finish_job(&self, id: JobId, end: &JobEnd, now_ms: i64, ttl_ms: i64) -> Result<()> {
        let (state, from, error, audio_ms, sha): (
            JobState,
            &str,
            Option<&str>,
            Option<i64>,
            Option<&str>,
        ) = match end {
            JobEnd::Succeeded {
                audio_ms,
                output_sha256,
            } => (
                JobState::Succeeded,
                "'running'",
                None,
                Some(to_i64(*audio_ms)),
                Some(output_sha256.as_str()),
            ),
            JobEnd::Failed(code) => (
                JobState::Failed,
                "'running'",
                Some(code.as_str()),
                None,
                None,
            ),
            JobEnd::Cancelled => (
                JobState::Cancelled,
                "'queued', 'running'",
                Some(ErrorCode::Cancelled.as_str()),
                None,
                None,
            ),
        };
        let n = self.conn().execute(
            &format!(
                "UPDATE jobs SET state = ?2, text = NULL, error_code = ?3, audio_ms = ?4,
                                 output_sha256 = ?5, finished_at_ms = ?6, expires_at_ms = ?7
                 WHERE id = ?1 AND state IN ({from})"
            ),
            params![
                id.to_string(),
                state.as_str(),
                error,
                audio_ms,
                sha,
                now_ms,
                now_ms.saturating_add(ttl_ms),
            ],
        )?;
        if n == 0 {
            return Err(StoreError::Conflict(format!("job cannot become {state}")));
        }
        Ok(())
    }

    /// running -> queued (crash recovery at startup).
    pub fn requeue_job(&self, id: JobId) -> Result<()> {
        let n = self.conn().execute(
            "UPDATE jobs SET state = 'queued', chunks_done = 0, started_at_ms = NULL,
                             first_audio_at_ms = NULL
             WHERE id = ?1 AND state = 'running'",
            [id.to_string()],
        )?;
        if n == 0 {
            return Err(StoreError::Conflict("job is not running".into()));
        }
        Ok(())
    }

    /// Delete an ended job's row and chunk rows.
    pub fn delete_job(&self, id: JobId) -> Result<()> {
        let n = self.conn().execute(
            "DELETE FROM jobs WHERE id = ?1 AND state NOT IN ('queued', 'running')",
            [id.to_string()],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound);
        }
        Ok(())
    }

    /// Ended jobs whose retention has run out.
    pub fn expired_jobs(&self, now_ms: i64) -> Result<Vec<JobId>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id FROM jobs WHERE expires_at_ms IS NOT NULL AND expires_at_ms <= ?1
             AND state NOT IN ('queued', 'running') ORDER BY id",
        )?;
        let rows = stmt.query_map([now_ms], |r| parse(r.get(0)?, "job id"))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn job_exists(&self, id: JobId) -> Result<bool> {
        let n: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM jobs WHERE id = ?1",
            [id.to_string()],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}
