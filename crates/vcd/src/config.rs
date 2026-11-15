//! `vcd.toml`. Every key has a default; unknown keys are an error.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: Server,
    pub storage: Storage,
    pub engine: EngineCfg,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Server {
    /// Must be a loopback address: v1 serves this machine only.
    pub listen: SocketAddr,
    pub max_body_bytes: usize,
    pub max_job_chars: usize,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Storage {
    pub data_dir: PathBuf,
    /// How long finished jobs and their audio are kept.
    pub output_ttl_hours: u32,
    /// How often expired jobs are swept.
    pub sweep_interval_minutes: u32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EngineCfg {
    /// Holds `files/` (model files) and `runtime/` (the unpacked CrispASR release), as
    /// laid out by `cargo xtask fetch-models`.
    pub models_dir: PathBuf,
    /// Overrides the recipe's thread count.
    pub threads: Option<u32>,
    pub nice: i32,
    pub queue_capacity: usize,
    pub inter_chunk_gap_ms: u32,
    pub crash_retries: u32,
    /// CrispASR's audible AI disclaimer before each chunk. Off by the owner's decision
    /// for v1; the inaudible watermark is always on.
    pub spoken_disclaimer: bool,
}

impl Default for Server {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8740)),
            max_body_bytes: 25_000_000,
            max_job_chars: 2_000,
        }
    }
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            output_ttl_hours: 24,
            sweep_interval_minutes: 10,
        }
    }
}

impl Default for EngineCfg {
    fn default() -> Self {
        Self {
            models_dir: PathBuf::from("models"),
            threads: None,
            nice: 10,
            queue_capacity: 16,
            inter_chunk_gap_ms: 200,
            crash_retries: 1,
            spoken_disclaimer: false,
        }
    }
}

/// `$XDG_DATA_HOME/voice-clone/vcd`, else `~/.local/share/voice-clone/vcd`.
fn default_data_dir() -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("voice-clone").join("vcd")
}

impl Config {
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let cfg = match path {
            Some(p) => {
                let text = std::fs::read_to_string(p)
                    .with_context(|| format!("reading {}", p.display()))?;
                toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?
            }
            None => Config::default(),
        };
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.server.listen.ip().is_loopback() {
            bail!(
                "listen address {} is not loopback; v1 serves this machine only",
                self.server.listen
            );
        }
        if self.storage.output_ttl_hours == 0 || self.storage.sweep_interval_minutes == 0 {
            bail!("output_ttl_hours and sweep_interval_minutes must be positive");
        }
        if self.engine.queue_capacity == 0 {
            bail!("queue_capacity must be positive");
        }
        if !(0..=19).contains(&self.engine.nice) {
            bail!("nice must be 0-19");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_local() {
        let c = Config::default();
        c.validate().unwrap();
        assert_eq!(c.server.listen.to_string(), "127.0.0.1:8740");
        assert_eq!(c.storage.output_ttl_hours, 24);
        assert!(!c.engine.spoken_disclaimer);
    }

    #[test]
    fn parses_partial_files_and_rejects_unknown_keys() {
        let c: Config = toml::from_str("[engine]\nthreads = 4\n").unwrap();
        assert_eq!(c.engine.threads, Some(4));
        assert_eq!(c.engine.nice, 10);
        assert!(toml::from_str::<Config>("[engine]\nthreadz = 4\n").is_err());
        assert!(toml::from_str::<Config>("[nope]\n").is_err());
    }

    #[test]
    fn refuses_non_loopback_listen() {
        let mut c = Config::default();
        c.server.listen = "0.0.0.0:8740".parse().unwrap();
        assert!(c.validate().is_err());
        c.server.listen = "[::1]:8740".parse().unwrap();
        c.validate().unwrap();
    }
}
