#![allow(clippy::unwrap_used)]

use vc_core::{ConsentId, ErrorCode, JobId, JobState, Language, Rate, VoiceId};
use vc_store::{ChunkRow, GENESIS_HASH, JobEnd, NewConsent, NewJob, Store, StoreError, VoiceRow};

const TTL: i64 = 24 * 3600 * 1000;

fn consent(store: &Store, now: i64) -> ConsentId {
    store
        .insert_consent(
            NewConsent {
                speaker_name: "Owner".into(),
                relationship: "self".into(),
                attestation: "This is my own voice.".into(),
                languages: vec![Language::En, Language::Hi],
            },
            now,
        )
        .unwrap()
        .id
}

fn voice(store: &Store) -> VoiceId {
    let c = consent(store, 1);
    let v = VoiceRow {
        id: VoiceId::generate(),
        consent_id: c,
        name: "me".into(),
        reference_language: Language::En,
        ref_sample_rate: 16_000,
        ref_ms: 11_000,
        ref_sha256: "ab".repeat(32),
        created_at_ms: 2,
        ref_cleaned: true,
    };
    store.insert_voice(&v).unwrap();
    v.id
}

fn job(store: &Store, voice: VoiceId, now: i64) -> JobId {
    let id = JobId::generate();
    store
        .insert_job(
            &NewJob {
                id,
                voice_id: voice,
                language: Language::Hi,
                text: "secret text".into(),
                text_sha256: "cd".repeat(32),
                chunks_total: 2,
                rate: Rate::NATIVE,
                polish: true,
            },
            now,
        )
        .unwrap();
    id
}

fn chunk(idx: usize) -> ChunkRow {
    ChunkRow {
        idx,
        words: 3,
        chars: 12,
        audio_ms: 900,
        peak_rss_kib: 1_000_000,
        wall_ms: 7_000,
        out_sha256: "ef".repeat(32),
    }
}

#[test]
fn consent_chain_links_and_verifies() {
    let s = Store::open_in_memory().unwrap();
    assert_eq!(s.consent_chain_head().unwrap(), GENESIS_HASH);
    let a = s.get_consent(consent(&s, 10)).unwrap();
    let b = s.get_consent(consent(&s, 20)).unwrap();
    assert_eq!(a.seq, 1);
    assert_eq!(a.prev_hash, GENESIS_HASH);
    assert_eq!(b.prev_hash, a.record_hash);
    assert_eq!(a.record_hash, a.compute_hash());
    assert_eq!(s.verify_consent_chain().unwrap(), b.record_hash);
    assert_eq!(s.consent_chain_head().unwrap(), b.record_hash);
}

#[test]
fn consents_cannot_be_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let s = Store::open(&path).unwrap();
    let id = consent(&s, 10);
    drop(s);
    let raw = rusqlite::Connection::open(&path).unwrap();
    assert!(
        raw.execute("UPDATE consents SET attestation = 'forged'", [])
            .is_err()
    );
    assert!(raw.execute("DELETE FROM consents", []).is_err());
    let s = Store::open(&path).unwrap();
    assert!(s.get_consent(id).is_ok());
    s.verify_consent_chain().unwrap();
}

#[test]
fn reopening_keeps_data_and_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let v = {
        let s = Store::open(&path).unwrap();
        voice(&s)
    };
    let s = Store::open(&path).unwrap();
    assert_eq!(s.get_voice(v).unwrap().ref_ms, 11_000);
}

#[test]
fn job_lifecycle_success_clears_text_and_sets_expiry() {
    let s = Store::open_in_memory().unwrap();
    let v = voice(&s);
    let j = job(&s, v, 100);
    let row = s.get_job(j).unwrap();
    assert_eq!(row.state, JobState::Queued);
    assert_eq!(row.text.as_deref(), Some("secret text"));

    s.start_job(j, 200).unwrap();
    s.record_chunk(j, &chunk(0), 300).unwrap();
    // Out of order is refused.
    assert!(matches!(
        s.record_chunk(j, &chunk(0), 301),
        Err(StoreError::Conflict(_))
    ));
    s.record_chunk(j, &chunk(1), 400).unwrap();
    s.finish_job(
        j,
        &JobEnd::Succeeded {
            audio_ms: 2_000,
            output_sha256: "aa".repeat(32),
        },
        500,
        TTL,
    )
    .unwrap();

    let row = s.get_job(j).unwrap();
    assert_eq!(row.state, JobState::Succeeded);
    assert_eq!(row.text, None);
    assert_eq!(row.chunks_done, 2);
    assert_eq!(row.attempts, 1);
    assert_eq!(row.started_at_ms, Some(200));
    assert_eq!(row.first_audio_at_ms, Some(300));
    assert_eq!(row.expires_at_ms, Some(500 + TTL));
    assert_eq!(s.chunks(j).unwrap().len(), 2);
}

#[test]
fn illegal_transitions_are_refused() {
    let s = Store::open_in_memory().unwrap();
    let v = voice(&s);
    let j = job(&s, v, 1);
    assert!(
        s.finish_job(j, &JobEnd::Failed(ErrorCode::EngineCrashed), 2, TTL)
            .is_err()
    );
    assert!(s.requeue_job(j).is_err());
    s.finish_job(j, &JobEnd::Cancelled, 2, TTL).unwrap();
    assert!(s.start_job(j, 3).is_err());
    assert!(s.finish_job(j, &JobEnd::Cancelled, 3, TTL).is_err());
    let row = s.get_job(j).unwrap();
    assert_eq!(row.state, JobState::Cancelled);
    assert_eq!(row.error_code, Some(ErrorCode::Cancelled));
}

#[test]
fn requeue_resets_progress_and_restart_counts_attempts() {
    let s = Store::open_in_memory().unwrap();
    let v = voice(&s);
    let j = job(&s, v, 1);
    s.start_job(j, 2).unwrap();
    s.record_chunk(j, &chunk(0), 3).unwrap();
    s.requeue_job(j).unwrap();
    let row = s.get_job(j).unwrap();
    assert_eq!(
        (row.state, row.chunks_done, row.first_audio_at_ms),
        (JobState::Queued, 0, None)
    );
    s.start_job(j, 4).unwrap();
    assert_eq!(s.get_job(j).unwrap().attempts, 2);
    assert!(s.chunks(j).unwrap().is_empty());
    assert_eq!(s.jobs_in_state(JobState::Running).unwrap().len(), 1);
}

#[test]
fn voice_in_use_cannot_be_deleted_and_deleted_voice_takes_no_jobs() {
    let s = Store::open_in_memory().unwrap();
    let v = voice(&s);
    let j = job(&s, v, 1);
    assert!(matches!(
        s.tombstone_voice(v, 5),
        Err(StoreError::Conflict(_))
    ));
    s.finish_job(j, &JobEnd::Cancelled, 2, TTL).unwrap();
    s.tombstone_voice(v, 5).unwrap();
    assert!(matches!(s.get_voice(v), Err(StoreError::NotFound)));
    assert!(s.list_voices().unwrap().is_empty());
    assert!(matches!(s.tombstone_voice(v, 6), Err(StoreError::NotFound)));
    let new = NewJob {
        id: JobId::generate(),
        voice_id: v,
        language: Language::En,
        text: "x".into(),
        text_sha256: "00".repeat(32),
        chunks_total: 1,
        rate: Rate::NATIVE,
        polish: false,
    };
    assert!(matches!(s.insert_job(&new, 7), Err(StoreError::NotFound)));
}

#[test]
fn expiry_lists_only_ended_jobs_past_their_ttl() {
    let s = Store::open_in_memory().unwrap();
    let v = voice(&s);
    let done = job(&s, v, 1);
    let live = job(&s, v, 1);
    s.finish_job(done, &JobEnd::Cancelled, 1_000, TTL).unwrap();
    assert!(s.expired_jobs(1_000 + TTL - 1).unwrap().is_empty());
    assert_eq!(s.expired_jobs(1_000 + TTL).unwrap(), vec![done]);
    s.delete_job(done).unwrap();
    assert!(!s.job_exists(done).unwrap());
    // A queued job can never be deleted through this path.
    assert!(s.delete_job(live).is_err());
}

#[test]
fn a_job_keeps_its_rate() {
    let s = Store::open_in_memory().unwrap();
    let v = voice(&s);
    let id = JobId::generate();
    let rate = Rate::from_f64(1.2).unwrap();
    s.insert_job(
        &NewJob {
            id,
            voice_id: v,
            language: Language::En,
            text: "hi".into(),
            text_sha256: "00".repeat(32),
            chunks_total: 1,
            rate,
            polish: false,
        },
        1,
    )
    .unwrap();
    assert_eq!(s.get_job(id).unwrap().rate, rate);
    assert_eq!(s.jobs_in_state(JobState::Queued).unwrap()[0].rate, rate);
}

#[test]
fn a_v1_database_is_migrated_and_its_jobs_keep_the_native_rate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vcd.sqlite3");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("../src/migrations/0001_init.sql"))
            .unwrap();
        c.pragma_update(None, "user_version", 1).unwrap();
    }
    let s = Store::open(&path).unwrap();
    let v = voice(&s);
    let j = job(&s, v, 1);
    assert_eq!(s.get_job(j).unwrap().rate, Rate::NATIVE);
    drop(s);
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        let version: i64 = c
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
        // A v1 job row (no rate given) reads back at the native rate.
        c.execute(
            "INSERT INTO jobs (id, voice_id, state, language, text, text_sha256, chunks_total,
                               created_at_ms)
             VALUES (?1, ?2, 'queued', 'en', 'x', ?3, 1, 1)",
            rusqlite::params![
                JobId::generate().to_string(),
                v.to_string(),
                "00".repeat(32)
            ],
        )
        .unwrap();
        // The schema refuses rates outside the accepted range.
        assert!(
            c.execute(
                "UPDATE jobs SET rate_milli = 2500 WHERE id = ?1",
                [j.to_string()]
            )
            .is_err()
        );
    }
    let s = Store::open(&path).unwrap();
    assert!(
        s.jobs_in_state(JobState::Queued)
            .unwrap()
            .iter()
            .all(|r| r.rate == Rate::NATIVE)
    );
}

#[test]
fn a_v2_database_is_migrated_and_its_rows_were_neither_cleaned_nor_polished() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vcd.sqlite3");
    let (consent_id, voice_id, job_id) = (
        ConsentId::generate(),
        VoiceId::generate(),
        JobId::generate(),
    );
    {
        // A v2 database with rows as a v2 vcd wrote them, without the new columns.
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(include_str!("../src/migrations/0001_init.sql"))
            .unwrap();
        c.execute_batch(include_str!("../src/migrations/0002_job_rate.sql"))
            .unwrap();
        c.pragma_update(None, "user_version", 2).unwrap();
        c.execute(
            "INSERT INTO consents (id, seq, speaker_name, relationship, attestation, languages,
                                   created_at_ms, prev_hash, record_hash)
             VALUES (?1, 1, 'Owner', 'self', 'This is my own voice.', 'en', 1, ?2, ?3)",
            rusqlite::params![consent_id.to_string(), GENESIS_HASH, "11".repeat(32)],
        )
        .unwrap();
        c.execute(
            "INSERT INTO voices (id, consent_id, name, reference_language, ref_sample_rate,
                                 ref_ms, ref_sha256, created_at_ms)
             VALUES (?1, ?2, 'me', 'en', 16000, 11000, ?3, 2)",
            rusqlite::params![
                voice_id.to_string(),
                consent_id.to_string(),
                "ab".repeat(32)
            ],
        )
        .unwrap();
        c.execute(
            "INSERT INTO jobs (id, voice_id, state, language, text, text_sha256, chunks_total,
                               created_at_ms, rate_milli)
             VALUES (?1, ?2, 'queued', 'en', 'x', ?3, 1, 1, 1200)",
            rusqlite::params![job_id.to_string(), voice_id.to_string(), "00".repeat(32)],
        )
        .unwrap();
    }
    let s = Store::open(&path).unwrap();
    assert!(!s.get_voice(voice_id).unwrap().ref_cleaned);
    let j = s.get_job(job_id).unwrap();
    assert!(!j.polish);
    assert_eq!(j.rate, Rate::from_f64(1.2).unwrap());
    // New rows keep what they were given.
    let v = voice(&s);
    assert!(s.get_voice(v).unwrap().ref_cleaned);
    assert!(s.get_job(job(&s, v, 3)).unwrap().polish);
    drop(s);
    let c = rusqlite::Connection::open(&path).unwrap();
    let version: i64 = c
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(version, 3);
    assert!(
        c.execute(
            "UPDATE jobs SET polish = 2 WHERE id = ?1",
            [job_id.to_string()]
        )
        .is_err()
    );
}
