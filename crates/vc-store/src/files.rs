//! The data directory: owner-only directories and files for references and job outputs.
//!
//! ```text
//! <data_dir>/            0700
//!   vcd.sqlite3
//!   voices/<voice_id>/ref.wav      normalized mono reference (0600)
//!   jobs/<job_id>/                 engine work dir and outputs (0700)
//!     chunk-0000.wav ...  out.wav  engine logs, consent.jsonl
//!   engine/                        the warm engine's private dir (0700), wiped whenever
//!                                  it starts or stops: HOME, caches, a link to the
//!                                  voice it serves, its log and consent.jsonl
//! ```
//!
//! Encryption at rest is a follow-up; until then the directory permissions are the
//! protection, and nothing here is ever world- or group-readable.

use std::fs::{DirBuilder, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use vc_core::{JobId, VoiceId};

#[derive(Debug, Clone)]
pub struct DataDir {
    root: PathBuf,
}

impl DataDir {
    /// Create (or tighten) the data directory layout.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        let d = Self {
            root: root.to_owned(),
        };
        for dir in [d.root.clone(), d.root.join("voices"), d.root.join("jobs")] {
            private_dir(&dir)?;
        }
        Ok(d)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn db_path(&self) -> PathBuf {
        self.root.join("vcd.sqlite3")
    }

    pub fn voice_dir(&self, id: VoiceId) -> PathBuf {
        self.root.join("voices").join(id.to_string())
    }

    pub fn reference_path(&self, id: VoiceId) -> PathBuf {
        self.voice_dir(id).join("ref.wav")
    }

    /// Owned by the warm engine backend, which creates and wipes it.
    pub fn engine_dir(&self) -> PathBuf {
        self.root.join("engine")
    }

    pub fn job_dir(&self, id: JobId) -> PathBuf {
        self.root.join("jobs").join(id.to_string())
    }

    pub fn chunk_path(&self, id: JobId, idx: usize) -> PathBuf {
        self.job_dir(id).join(format!("chunk-{idx:04}.wav"))
    }

    pub fn output_path(&self, id: JobId) -> PathBuf {
        self.job_dir(id).join("out.wav")
    }

    /// Ids of every job directory on disk, for sweeping orphans.
    pub fn job_dirs(&self) -> std::io::Result<Vec<JobId>> {
        let mut ids = Vec::new();
        for entry in std::fs::read_dir(self.root.join("jobs"))? {
            if let Some(id) = entry?.file_name().to_str().and_then(|s| s.parse().ok()) {
                ids.push(id);
            }
        }
        Ok(ids)
    }
}

/// Create `dir` (and parents) with mode 0700, and tighten it if it already exists.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// Write `bytes` to `path` atomically (temp file + rename) with mode 0600.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut f: File = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// Remove a directory tree; a missing directory is fine.
pub fn remove_tree(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn layout_is_owner_only() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("data");
        let d = DataDir::open(&root).unwrap();
        assert_eq!(mode(&root), 0o700);
        assert_eq!(mode(&root.join("voices")), 0o700);
        let v = VoiceId::generate();
        private_dir(&d.voice_dir(v)).unwrap();
        write_private(&d.reference_path(v), b"x").unwrap();
        assert_eq!(mode(&d.reference_path(v)), 0o600);
        assert_eq!(mode(&d.voice_dir(v)), 0o700);
    }

    #[test]
    fn tightens_an_existing_loose_directory() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("data");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        DataDir::open(&root).unwrap();
        assert_eq!(mode(&root), 0o700);
    }

    #[test]
    fn lists_job_dirs_and_ignores_strays() {
        let t = tempfile::tempdir().unwrap();
        let d = DataDir::open(t.path()).unwrap();
        let j = JobId::generate();
        private_dir(&d.job_dir(j)).unwrap();
        private_dir(&t.path().join("jobs").join("not-a-job")).unwrap();
        assert_eq!(d.job_dirs().unwrap(), vec![j]);
        remove_tree(&d.job_dir(j)).unwrap();
        remove_tree(&d.job_dir(j)).unwrap();
        assert!(d.job_dirs().unwrap().is_empty());
    }
}
