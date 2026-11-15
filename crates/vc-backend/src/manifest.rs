//! Model recipe manifests and the runtime pin, both TOML files committed to the repo and
//! embedded into the binary at build time.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vc_core::Language;

/// `models/manifests/turbo2.toml`.
pub const TURBO2_MANIFEST: &str = include_str!("../../../models/manifests/turbo2.toml");
/// `third_party/crispasr/PIN.toml`.
pub const CRISPASR_PIN: &str = include_str!("../../../third_party/crispasr/PIN.toml");

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeManifest {
    pub id: String,
    pub kind: String,
    pub runtime: String,
    pub files: Vec<ModelFile>,
    pub engine: EngineSettings,
    pub budget: Budget,
    pub reference: ReferenceSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFile {
    pub role: String,
    pub name: String,
    pub url: String,
    pub bytes: u64,
    pub sha256: String,
    pub license: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngineSettings {
    pub backend: String,
    pub tts_steps: u32,
    pub threads: u32,
    pub seed: u64,
    pub sample_rate_hz: u32,
    pub languages: Vec<Language>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub ram_budget_kib: u64,
    pub max_chunk_words: usize,
    pub max_chunk_chars: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceSettings {
    pub min_s: f64,
    pub max_s: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimePin {
    pub name: String,
    pub version: String,
    pub commit: String,
    pub license: String,
    pub source: String,
    #[serde(rename = "linux-x86_64")]
    pub linux_x86_64: RuntimeArtifact,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeArtifact {
    pub url: String,
    pub bytes: u64,
    pub sha256: String,
    pub dir: String,
    pub binary: String,
    pub binary_bytes: u64,
    pub binary_sha256: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    #[error("manifest parse: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("{path}: missing ({source})")]
    Missing {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {actual} bytes, expected {expected}")]
    Size {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    #[error("{path}: sha256 {actual}, expected {expected}")]
    Hash {
        path: PathBuf,
        expected: String,
        actual: String,
    },
}

impl RecipeManifest {
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let m: RecipeManifest = toml::from_str(text)?;
        m.validate()?;
        Ok(m)
    }

    pub fn turbo2() -> Self {
        // The embedded manifest is checked by a unit test, so this cannot fail at runtime.
        #[allow(clippy::expect_used)]
        Self::parse(TURBO2_MANIFEST).expect("embedded turbo2 manifest is valid")
    }

    fn validate(&self) -> Result<(), ManifestError> {
        let invalid = |m: &str| Err(ManifestError::Invalid(format!("{}: {m}", self.id)));
        if self.kind != "crispasr-cli" {
            return invalid("kind must be crispasr-cli");
        }
        for role in ["t3", "s3gen"] {
            if self.files.iter().filter(|f| f.role == role).count() != 1 {
                return invalid("needs exactly one t3 and one s3gen file");
            }
        }
        for f in &self.files {
            let plain = !f.name.is_empty()
                && f.name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                && !f.name.starts_with('.');
            if !plain {
                return invalid("file names must be plain names");
            }
            if f.sha256.len() != 64 || !f.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
                return invalid("sha256 must be 64 hex digits");
            }
        }
        if self.engine.languages.is_empty() {
            return invalid("no languages");
        }
        if self.budget.max_chunk_words == 0 || self.budget.max_chunk_chars == 0 {
            return invalid("chunk caps must be positive");
        }
        if self.engine.threads == 0 || self.engine.tts_steps == 0 {
            return invalid("threads and tts_steps must be positive");
        }
        if !(self.reference.min_s > 0.0 && self.reference.min_s < self.reference.max_s) {
            return invalid("reference limits");
        }
        Ok(())
    }

    pub fn file(&self, role: &str) -> Option<&ModelFile> {
        self.files.iter().find(|f| f.role == role)
    }

    /// Check every model file under `model_dir` for size and sha256.
    pub fn verify_files(&self, model_dir: &Path) -> Result<(), VerifyError> {
        for f in &self.files {
            verify_file(&model_dir.join(&f.name), f.bytes, &f.sha256)?;
        }
        Ok(())
    }
}

impl RuntimePin {
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        Ok(toml::from_str(text)?)
    }

    pub fn crispasr() -> Self {
        #[allow(clippy::expect_used)]
        Self::parse(CRISPASR_PIN).expect("embedded CrispASR pin is valid")
    }

    /// Install directory of the unpacked release under `models_root`.
    pub fn install_dir(&self, models_root: &Path) -> PathBuf {
        models_root
            .join("runtime")
            .join(format!("{}-{}", self.name, self.version))
    }

    pub fn binary_path(&self, models_root: &Path) -> PathBuf {
        self.install_dir(models_root)
            .join(&self.linux_x86_64.dir)
            .join(&self.linux_x86_64.binary)
    }

    /// Check the unpacked binary under `models_root` for size and sha256.
    pub fn verify_binary(&self, models_root: &Path) -> Result<(), VerifyError> {
        let a = &self.linux_x86_64;
        verify_file(
            &self.binary_path(models_root),
            a.binary_bytes,
            &a.binary_sha256,
        )
    }
}

/// Streaming sha256 of a file.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn verify_file(path: &Path, bytes: u64, sha256: &str) -> Result<(), VerifyError> {
    let meta = std::fs::metadata(path).map_err(|source| VerifyError::Missing {
        path: path.to_owned(),
        source,
    })?;
    if meta.len() != bytes {
        return Err(VerifyError::Size {
            path: path.to_owned(),
            expected: bytes,
            actual: meta.len(),
        });
    }
    let actual = sha256_file(path).map_err(|source| VerifyError::Missing {
        path: path.to_owned(),
        source,
    })?;
    if !actual.eq_ignore_ascii_case(sha256) {
        return Err(VerifyError::Hash {
            path: path.to_owned(),
            expected: sha256.to_owned(),
            actual,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_turbo2_manifest_matches_the_chosen_recipe() {
        let m = RecipeManifest::turbo2();
        assert_eq!(m.id, "turbo2");
        assert_eq!(m.runtime, "third_party/crispasr/PIN.toml");
        assert_eq!(m.file("t3").unwrap().name, "chatterbox-v3-t3-q4_k.gguf");
        let s3 = m.file("s3gen").unwrap();
        assert_eq!(s3.name, "chatterbox-turbo-s3gen-q4_k.gguf");
        assert_eq!(
            s3.sha256,
            "1c16251e6722c46500ebddd92be495e0dba160c31169f4b64d833acba819b060"
        );
        assert_eq!(m.engine.tts_steps, 2);
        assert_eq!(m.engine.threads, 6);
        assert_eq!(m.engine.seed, 0);
        assert_eq!(m.engine.env.get("CRISPASR_S3GEN_FASTCONV").unwrap(), "0");
        assert_eq!(m.budget.max_chunk_words, 19);
        assert_eq!(m.budget.ram_budget_kib, 1_100_000);
        assert_eq!(m.engine.languages, [Language::En, Language::Hi]);
        for f in &m.files {
            assert!(f.url.ends_with(&format!("/{}", f.name)));
            assert!(f.url.starts_with("https://huggingface.co/cstr/"));
        }
    }

    #[test]
    fn embedded_pin_is_v0_8_53() {
        let p = RuntimePin::crispasr();
        assert_eq!(p.version, "0.8.53");
        assert_eq!(p.commit, "1694d8ef516799b290c6da5fe80803b4af2771b0");
        assert_eq!(
            p.linux_x86_64.sha256,
            "3396d49473642d1097b667270b54125279bdca740b735e15c3c96c1b32f787c0"
        );
        assert!(p.linux_x86_64.url.contains("/v0.8.53/"));
        assert_eq!(
            p.binary_path(Path::new("models")),
            Path::new("models/runtime/crispasr-0.8.53/crispasr-linux-x86_64/crispasr")
        );
    }

    #[test]
    fn rejects_path_like_file_names_and_bad_hashes() {
        let good = TURBO2_MANIFEST;
        let traversal = good.replace(
            "name = \"chatterbox-v3-t3-q4_k.gguf\"",
            "name = \"../chatterbox-v3-t3-q4_k.gguf\"",
        );
        assert!(RecipeManifest::parse(&traversal).is_err());
        let bad_hash = good.replace(
            "fb63d7c53c4f8bb07286d67709c4d4200fa63b4de3476a4558f37964ce8e5aa6",
            "xyz",
        );
        assert!(RecipeManifest::parse(&bad_hash).is_err());
        let unknown = format!("{good}\n[extra]\nx = 1\n");
        assert!(RecipeManifest::parse(&unknown).is_err());
    }

    #[test]
    fn verify_file_checks_size_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.bin");
        std::fs::write(&p, b"abc").unwrap();
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert!(verify_file(&p, 3, sha).is_ok());
        assert!(matches!(
            verify_file(&p, 4, sha),
            Err(VerifyError::Size { .. })
        ));
        assert!(matches!(
            verify_file(&p, 3, &"0".repeat(64)),
            Err(VerifyError::Hash { .. })
        ));
        assert!(matches!(
            verify_file(&dir.path().join("nope"), 3, sha),
            Err(VerifyError::Missing { .. })
        ));
    }
}
