//! The cold backend: the pinned CrispASR CLI, one process per chunk.
//!
//! Every chunk pays for loading the models and encoding the reference (about 6 s on an
//! idle laptop, 15 s or more under load), which is why vcd normally runs
//! [`crate::CrispWarmBackend`]. This one stays as the `engine.warm = false` fallback: a
//! fresh process per chunk keeps every chunk's peak RSS at the measured 1.00-1.07 GB,
//! makes cancel a process-group kill, and contains engine crashes.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use tokio_util::sync::CancellationToken;

use crate::manifest::{RecipeManifest, RuntimePin};
use crate::proc::{self, EngineCommand, ProcError};
use crate::{
    Backend, BackendError, BackendInfo, BoxFuture, Capabilities, ChunkOutput, ChunkRequest,
    ChunkStats, ModelInfo, RuntimeInfo,
};

#[derive(Debug, Clone)]
pub struct CrispOptions {
    /// The `crispasr` binary.
    pub runtime_bin: PathBuf,
    /// Directory holding the manifest's model files.
    pub model_dir: PathBuf,
    /// Overrides the manifest's thread count.
    pub threads: Option<u32>,
    /// Minimum niceness of the engine (the laptop is shared).
    pub nice: i32,
    /// Extra environment for the engine. Only tests use this, to steer `fake-engine`.
    pub extra_env: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct CrispCliBackend {
    manifest: RecipeManifest,
    options: CrispOptions,
    capabilities: Capabilities,
    info: BackendInfo,
}

/// What the API may promise for a CrispASR backend of this recipe.
pub(crate) fn capabilities(manifest: &RecipeManifest) -> Capabilities {
    Capabilities {
        backend_id: manifest.id.clone(),
        languages: manifest.engine.languages.clone(),
        sample_rate_hz: manifest.engine.sample_rate_hz,
        channels: 1,
        max_chunk_words: manifest.budget.max_chunk_words,
        max_chunk_chars: manifest.budget.max_chunk_chars,
        concurrency: 1,
        ram_budget_kib: manifest.budget.ram_budget_kib,
        deterministic: true,
        watermark: "crispasr-builtin",
        reference_min_s: manifest.reference.min_s,
        reference_max_s: manifest.reference.max_s,
    }
}

pub(crate) fn backend_info(
    manifest: &RecipeManifest,
    pin: &RuntimePin,
    threads: u32,
) -> BackendInfo {
    BackendInfo {
        backend_id: manifest.id.clone(),
        runtime: Some(RuntimeInfo {
            name: pin.name.clone(),
            version: pin.version.clone(),
            commit: pin.commit.clone(),
            tarball_sha256: pin.linux_x86_64.sha256.clone(),
        }),
        models: manifest
            .files
            .iter()
            .map(|f| ModelInfo {
                role: f.role.clone(),
                name: f.name.clone(),
                sha256: f.sha256.clone(),
            })
            .collect(),
        seed: manifest.engine.seed,
        tts_steps: manifest.engine.tts_steps,
        threads,
    }
}

pub(crate) fn model_path(manifest: &RecipeManifest, model_dir: &Path, role: &str) -> PathBuf {
    let name = manifest.file(role).map_or("", |f| f.name.as_str());
    model_dir.join(name)
}

/// The engine environment: the manifest's variables, then HOME, the cache and TMPDIR
/// under `home` (so the engine neither loads unpinned cached models nor leaves
/// voice-derived files in shared places), then `extra`.
pub(crate) fn engine_env(
    manifest: &RecipeManifest,
    home: &Path,
    extra: &[(String, String)],
) -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = manifest
        .engine
        .env
        .iter()
        .map(|(k, v)| (k.into(), v.into()))
        .collect();
    env.push(("HOME".into(), home.to_owned().into()));
    env.push(("XDG_CACHE_HOME".into(), home.join("cache").into()));
    env.push(("TMPDIR".into(), home.join("tmp").into()));
    env.extend(extra.iter().map(|(k, v)| (k.into(), v.into())));
    env
}

impl CrispCliBackend {
    pub fn new(manifest: RecipeManifest, pin: &RuntimePin, options: CrispOptions) -> Self {
        let threads = options.threads.unwrap_or(manifest.engine.threads);
        let capabilities = capabilities(&manifest);
        let info = backend_info(&manifest, pin, threads);
        Self {
            manifest,
            options,
            capabilities,
            info,
        }
    }

    pub fn manifest(&self) -> &RecipeManifest {
        &self.manifest
    }

    fn model_path(&self, role: &str) -> PathBuf {
        model_path(&self.manifest, &self.options.model_dir, role)
    }

    /// The exact argv for one chunk. Text and paths are single argv elements passed to
    /// `execve`; no shell is involved.
    pub fn argv(&self, req: &ChunkRequest, output: &Path, consent_log: &Path) -> Vec<OsString> {
        let e = &self.manifest.engine;
        let mut a: Vec<OsString> = Vec::with_capacity(32);
        let mut push = |s: &dyn AsRef<std::ffi::OsStr>| a.push(s.as_ref().to_owned());
        push(&"--backend");
        push(&e.backend);
        push(&"-m");
        push(&self.model_path("t3"));
        push(&"--codec-model");
        push(&self.model_path("s3gen"));
        push(&"-t");
        push(&self.info.threads.to_string());
        push(&"-l");
        push(&req.language.code());
        if req.language != req.reference_language {
            // Cross-lingual: tell the engine what the reference is spoken in. For a
            // Hindi job from an English reference this runs T3 without guidance, which
            // measured better likeness and half the T3 cost.
            push(&"-sl");
            push(&req.reference_language.code());
        }
        push(&"--seed");
        push(&e.seed.to_string());
        push(&"--tts-steps");
        push(&e.tts_steps.to_string());
        push(&"--voice");
        push(&req.reference);
        push(&"--i-have-rights");
        push(&"--consent-log");
        push(&consent_log);
        push(&"--no-spoken-disclaimer");
        push(&"--accept-marking-responsibility");
        push(&"--tts");
        push(&req.text);
        push(&"--tts-output");
        push(&output);
        a
    }

    /// The complete engine environment. HOME, the cache and TMPDIR point into the job's
    /// private directory, so the engine neither loads unpinned cached models nor leaves
    /// voice-derived files in shared places.
    pub fn env(&self, req: &ChunkRequest) -> Vec<(OsString, OsString)> {
        engine_env(
            &self.manifest,
            &req.work_dir.join("engine-home"),
            &self.options.extra_env,
        )
    }

    async fn run_chunk(
        &self,
        req: &ChunkRequest,
        cancel: &CancellationToken,
    ) -> Result<ChunkOutput, BackendError> {
        let output = req
            .work_dir
            .join(format!("chunk-{:04}.engine.wav", req.idx));
        let log = req
            .work_dir
            .join(format!("chunk-{:04}.engine.log", req.idx));
        let consent_log = req.work_dir.join("consent.jsonl");
        let home = req.work_dir.join("engine-home");
        for dir in [home.join("cache"), home.join("tmp")] {
            std::fs::create_dir_all(&dir).map_err(|e| BackendError::Spawn(e.to_string()))?;
        }
        match std::fs::remove_file(&output) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(BackendError::Spawn(e.to_string())),
        }

        let cmd = EngineCommand {
            program: self.options.runtime_bin.clone(),
            args: self.argv(req, &output, &consent_log),
            env: self.env(req),
            cwd: req.work_dir.clone(),
            log,
            nice: self.options.nice,
        };
        let outcome = match proc::run(&cmd, cancel).await {
            Ok(o) => o,
            Err(ProcError::Cancelled(_)) => return Err(BackendError::Cancelled),
            Err(e) => return Err(BackendError::Spawn(e.to_string())),
        };
        let stats = ChunkStats {
            wall: outcome.wall,
            peak_rss_kib: outcome.peak_rss_kib,
        };
        if !outcome.exit.success() {
            return Err(BackendError::EngineCrashed {
                exit: outcome.exit,
                stats,
            });
        }
        let budget_kib = self.manifest.budget.ram_budget_kib;
        if outcome.peak_rss_kib > budget_kib {
            return Err(BackendError::MemoryBudgetExceeded { budget_kib, stats });
        }
        let pcm = vc_audio::read_engine_output(&output, self.manifest.engine.sample_rate_hz)
            .map_err(|e| BackendError::OutputInvalid {
                reason: e.to_string(),
                stats: stats.clone(),
            })?;
        // The caller stores its own canonical copy.
        let _ = std::fs::remove_file(&output);
        Ok(ChunkOutput { pcm, stats })
    }
}

impl Backend for CrispCliBackend {
    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn info(&self) -> &BackendInfo {
        &self.info
    }

    fn synthesize_chunk<'a>(
        &'a self,
        req: &'a ChunkRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ChunkOutput, BackendError>> {
        Box::pin(self.run_chunk(req, cancel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_core::Language;

    fn backend() -> CrispCliBackend {
        CrispCliBackend::new(
            RecipeManifest::turbo2(),
            &RuntimePin::crispasr(),
            CrispOptions {
                runtime_bin: "/opt/crispasr".into(),
                model_dir: "/m".into(),
                threads: None,
                nice: 10,
                extra_env: vec![],
            },
        )
    }

    fn req(language: Language) -> ChunkRequest {
        ChunkRequest {
            idx: 3,
            text: "-l starts like a flag".into(),
            language,
            reference: "/r/ref.wav".into(),
            reference_language: Language::En,
            work_dir: "/w".into(),
        }
    }

    fn strings(v: Vec<OsString>) -> Vec<String> {
        v.into_iter().map(|s| s.into_string().unwrap()).collect()
    }

    #[test]
    fn english_argv_is_the_turbo2_recipe() {
        let b = backend();
        let argv = strings(b.argv(
            &req(Language::En),
            Path::new("/w/o.wav"),
            Path::new("/w/c.jsonl"),
        ));
        assert_eq!(
            argv,
            [
                "--backend",
                "chatterbox",
                "-m",
                "/m/chatterbox-v3-t3-q4_k.gguf",
                "--codec-model",
                "/m/chatterbox-turbo-s3gen-q4_k.gguf",
                "-t",
                "6",
                "-l",
                "en",
                "--seed",
                "0",
                "--tts-steps",
                "2",
                "--voice",
                "/r/ref.wav",
                "--i-have-rights",
                "--consent-log",
                "/w/c.jsonl",
                "--no-spoken-disclaimer",
                "--accept-marking-responsibility",
                "--tts",
                "-l starts like a flag",
                "--tts-output",
                "/w/o.wav",
            ]
        );
    }

    #[test]
    fn hindi_from_an_english_reference_adds_source_language() {
        let b = backend();
        let argv = strings(b.argv(&req(Language::Hi), Path::new("/o"), Path::new("/c")));
        let l = argv.iter().position(|a| a == "-l").unwrap();
        assert_eq!(argv[l + 1..l + 4], ["hi", "-sl", "en"]);
    }

    #[test]
    fn env_is_isolated() {
        let env: Vec<(String, String)> = backend()
            .env(&req(Language::En))
            .into_iter()
            .map(|(k, v)| (k.into_string().unwrap(), v.into_string().unwrap()))
            .collect();
        assert_eq!(
            env,
            [
                ("CRISPASR_S3GEN_FASTCONV".into(), "0".into()),
                ("MALLOC_ARENA_MAX".into(), "1".into()),
                ("MALLOC_MMAP_THRESHOLD_".into(), "65536".into()),
                ("MALLOC_TRIM_THRESHOLD_".into(), "0".into()),
                ("HOME".into(), "/w/engine-home".into()),
                ("XDG_CACHE_HOME".into(), "/w/engine-home/cache".into()),
                ("TMPDIR".into(), "/w/engine-home/tmp".into()),
            ]
        );
    }

    #[test]
    fn capabilities_come_from_the_manifest() {
        let c = backend().capabilities().clone();
        assert_eq!(c.backend_id, "turbo2");
        assert_eq!(c.max_chunk_words, 19);
        assert_eq!(c.concurrency, 1);
        assert_eq!(c.sample_rate_hz, 24_000);
    }
}
