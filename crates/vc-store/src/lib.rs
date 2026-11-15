//! Persistent state of `vcd`: SQLite metadata (consents, voices, jobs, chunks) and an
//! owner-only data directory for references and outputs.

pub mod db;
pub mod files;

pub use db::{
    ChunkRow, ConsentRecord, GENESIS_HASH, JobEnd, JobRow, NewConsent, NewJob, Store, StoreError,
    VoiceRow,
};
pub use files::DataDir;
