//! `cargo xtask`: repository tasks.
//!
//! - `fetch-models`: download the turbo2 model files and the pinned CrispASR release into
//!   `models/` (gitignored), verifying size and sha256 of everything.
//! - `verify`: re-check what is already there.
//! - `splice`: build a GGUF file from two others (how a derived model file is made).

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail, ensure};
use clap::{Parser, Subcommand};
use vc_backend::manifest::{
    Derivation, ModelFile, RecipeManifest, RuntimePin, VerifyError, verify_file,
};

mod gguf;

/// Refuse to download when less than this is free on the target filesystem.
const MIN_FREE_BYTES: u64 = 5_000_000_000;

#[derive(Debug, Parser)]
#[command(name = "xtask")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Download and verify the model files and the engine runtime.
    FetchModels {
        #[arg(long, default_value = "models")]
        models_dir: PathBuf,
    },
    /// Verify the model files and the engine runtime.
    Verify {
        #[arg(long, default_value = "models")]
        models_dir: PathBuf,
    },
    /// Write BASE with every tensor whose name starts with PREFIX taken from DONOR.
    Splice {
        base: PathBuf,
        donor: PathBuf,
        out: PathBuf,
        prefix: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let manifest = RecipeManifest::turbo2();
    let pin = RuntimePin::crispasr();
    match cli.command {
        Cmd::FetchModels { models_dir } => fetch(&manifest, &pin, &models_dir),
        Cmd::Verify { models_dir } => {
            manifest.verify_files(&models_dir.join("files"))?;
            pin.verify_binary(&models_dir)?;
            println!(
                "verified {} files and {} {}",
                manifest.files.len(),
                pin.name,
                pin.version
            );
            Ok(())
        }
        Cmd::Splice {
            base,
            donor,
            out,
            prefix,
        } => {
            let n = splice_files(&base, &donor, &out, &prefix)?;
            println!("replaced {n} tensors; wrote {}", out.display());
            Ok(())
        }
    }
}

/// Build a derived model file: fetch its two published inputs (verified), splice them,
/// check the result against the file's own pin, then remove the inputs, which vcd never
/// loads.
fn derive(files_dir: &Path, f: &ModelFile, d: &Derivation) -> anyhow::Result<()> {
    ensure_free_space(files_dir, f.bytes + d.base.bytes + d.donor.bytes)?;
    for src in [&d.base, &d.donor] {
        let path = files_dir.join(&src.name);
        if verify_file(&path, src.bytes, &src.sha256).is_ok() {
            continue;
        }
        println!(
            "download {} ({} MB, input of {})",
            src.name,
            src.bytes / 1_000_000,
            f.name
        );
        download(&src.url, &path, src.bytes, &src.sha256)?;
    }
    println!(
        "build    {} ({} with the {}* tensors of {})",
        f.name, d.base.name, d.tensor_prefix, d.donor.name
    );
    let dest = files_dir.join(&f.name);
    splice_files(
        &files_dir.join(&d.base.name),
        &files_dir.join(&d.donor.name),
        &dest,
        &d.tensor_prefix,
    )?;
    if let Err(e) = verify_file(&dest, f.bytes, &f.sha256) {
        let _ = std::fs::remove_file(&dest);
        bail!("derived file failed verification: {e}");
    }
    for src in [&d.base, &d.donor] {
        std::fs::remove_file(files_dir.join(&src.name))?;
    }
    Ok(())
}

/// Splice two GGUF files into `out` (through a `.part` file).
fn splice_files(base: &Path, donor: &Path, out: &Path, prefix: &str) -> anyhow::Result<usize> {
    let (b, d) = (gguf::read(base)?, gguf::read(donor)?);
    let part = out.with_extension("part");
    let mut w = std::io::BufWriter::new(std::fs::File::create(&part)?);
    let n = gguf::splice(&b, &d, prefix, &mut w)?;
    std::io::Write::flush(&mut w)?;
    drop(w);
    std::fs::rename(&part, out)?;
    Ok(n)
}

fn fetch(manifest: &RecipeManifest, pin: &RuntimePin, root: &Path) -> anyhow::Result<()> {
    let files_dir = root.join("files");
    std::fs::create_dir_all(&files_dir)?;
    std::fs::create_dir_all(root.join("runtime"))?;

    for f in &manifest.files {
        let dest = files_dir.join(&f.name);
        if verify_file(&dest, f.bytes, &f.sha256).is_ok() {
            println!("ok       {}", f.name);
            continue;
        }
        match (&f.url, &f.derive) {
            (Some(url), _) => {
                ensure_free_space(&files_dir, f.bytes)?;
                println!(
                    "download {} ({} MB, {})",
                    f.name,
                    f.bytes / 1_000_000,
                    f.license
                );
                download(url, &dest, f.bytes, &f.sha256)?;
            }
            (None, Some(d)) => derive(&files_dir, f, d)?,
            (None, None) => bail!("{} has neither a url nor a derivation", f.name),
        }
    }

    let artifact = pin.host_artifact();
    match pin.verify_binary(root) {
        Ok(()) => println!("ok       {} {}", pin.name, pin.version),
        Err(_) => {
            ensure_free_space(&files_dir, artifact.bytes * 4)?;
            let install = pin.install_dir(root);
            let tarball = root
                .join("runtime")
                .join(format!("{}-{}.tar.gz", pin.name, pin.version));
            println!(
                "download {} {} ({} MB, {})",
                pin.name,
                pin.version,
                artifact.bytes / 1_000_000,
                pin.license
            );
            download(&artifact.url, &tarball, artifact.bytes, &artifact.sha256)?;
            if install.exists() {
                std::fs::remove_dir_all(&install)?;
            }
            std::fs::create_dir_all(&install)?;
            run(Command::new("tar")
                .arg("-xzf")
                .arg(&tarball)
                .arg("-C")
                .arg(&install)
                .arg("--no-same-owner"))?;
            std::fs::remove_file(&tarball)?;
            pin.verify_binary(root)
                .context("unpacked binary does not match the pin")?;
        }
    }
    println!("all verified under {}", root.display());
    Ok(())
}

/// Download `url` to `dest` through a `.part` file, then verify size and sha256.
fn download(url: &str, dest: &Path, bytes: u64, sha256: &str) -> anyhow::Result<()> {
    ensure!(url.starts_with("https://"), "refusing non-https url {url}");
    let part = dest.with_extension("part");
    run(Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--retry",
            "3",
        ])
        .args(["--proto", "=https", "--proto-redir", "=https"])
        .arg("--output")
        .arg(&part)
        .arg(url))?;
    match verify_file(&part, bytes, sha256) {
        Ok(()) => {}
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(describe(e));
        }
    }
    std::fs::rename(&part, dest)?;
    Ok(())
}

fn describe(e: VerifyError) -> anyhow::Error {
    anyhow::anyhow!("download failed verification: {e}")
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} failed: {status}", cmd.get_program());
    }
    Ok(())
}

fn ensure_free_space(dir: &Path, needed: u64) -> anyhow::Result<()> {
    let free = free_bytes(dir)?;
    if free < MIN_FREE_BYTES + needed {
        bail!(
            "only {} GB free on {}; need {} GB plus a {} GB reserve",
            free / 1_000_000_000,
            dir.display(),
            needed.div_ceil(1_000_000_000),
            MIN_FREE_BYTES / 1_000_000_000
        );
    }
    Ok(())
}

fn free_bytes(dir: &Path) -> anyhow::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes())?;
    // SAFETY: statvfs is plain data and `c` is a valid C string.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    ensure!(
        rc == 0,
        "statvfs {}: {}",
        dir.display(),
        std::io::Error::last_os_error()
    );
    // The statvfs field widths differ by platform (u32 blocks on macOS, u64 on Linux).
    #[allow(clippy::useless_conversion)]
    Ok(u64::from(st.f_bavail).saturating_mul(u64::from(st.f_frsize)))
}
