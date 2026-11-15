-- vcd schema v1. Timestamps are Unix milliseconds.

-- Consent records are append-only and hash-chained: record_hash covers the record and
-- the previous record's hash, so rewriting history is detectable.
CREATE TABLE consents (
    id TEXT PRIMARY KEY,
    seq INTEGER NOT NULL UNIQUE,
    speaker_name TEXT NOT NULL,
    relationship TEXT NOT NULL,
    attestation TEXT NOT NULL,
    languages TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    prev_hash TEXT NOT NULL,
    record_hash TEXT NOT NULL UNIQUE
) STRICT;

CREATE TRIGGER consents_no_update BEFORE UPDATE ON consents
BEGIN
    SELECT RAISE(ABORT, 'consents are append-only');
END;

CREATE TRIGGER consents_no_delete BEFORE DELETE ON consents
BEGIN
    SELECT RAISE(ABORT, 'consents are append-only');
END;

-- A deleted voice keeps a tombstone row (deleted_at_ms) for audit; its audio is gone.
CREATE TABLE voices (
    id TEXT PRIMARY KEY,
    consent_id TEXT NOT NULL REFERENCES consents (id),
    name TEXT NOT NULL,
    reference_language TEXT NOT NULL,
    ref_sample_rate INTEGER NOT NULL,
    ref_ms INTEGER NOT NULL,
    ref_sha256 TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    deleted_at_ms INTEGER
) STRICT;

-- Job text is kept only while the job can still run (for restart recovery) and is
-- cleared when the job ends; text_sha256 stays.
CREATE TABLE jobs (
    id TEXT PRIMARY KEY,
    voice_id TEXT NOT NULL REFERENCES voices (id),
    state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'cancelled')),
    language TEXT NOT NULL,
    text TEXT,
    text_sha256 TEXT NOT NULL,
    chunks_total INTEGER NOT NULL,
    chunks_done INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    error_code TEXT,
    audio_ms INTEGER,
    output_sha256 TEXT,
    created_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    first_audio_at_ms INTEGER,
    finished_at_ms INTEGER,
    expires_at_ms INTEGER,
    CHECK ((state IN ('queued', 'running')) = (text IS NOT NULL))
) STRICT;

CREATE INDEX jobs_by_state ON jobs (state, created_at_ms);
CREATE INDEX jobs_by_voice ON jobs (voice_id, state);

CREATE TABLE chunks (
    job_id TEXT NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    idx INTEGER NOT NULL,
    words INTEGER NOT NULL,
    chars INTEGER NOT NULL,
    audio_ms INTEGER NOT NULL,
    peak_rss_kib INTEGER NOT NULL,
    wall_ms INTEGER NOT NULL,
    out_sha256 TEXT NOT NULL,
    PRIMARY KEY (job_id, idx)
) STRICT;
