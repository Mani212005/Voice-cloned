//! Run one engine process: its own process group, a cleared environment, lowered CPU
//! priority, killed with its parent, reaped with `wait4` so the peak RSS comes from the
//! kernel (`ru_maxrss`) rather than from polling.

use std::ffi::OsString;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct EngineCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// The complete environment of the child; nothing is inherited.
    pub env: Vec<(OsString, OsString)>,
    pub cwd: PathBuf,
    /// stdout and stderr both go here.
    pub log: PathBuf,
    /// Minimum niceness of the child: it runs at this niceness or at the parent's,
    /// whichever is lower priority (0 = unchanged).
    pub nice: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitInfo {
    Code(i32),
    Signal(i32),
}

impl ExitInfo {
    pub fn success(self) -> bool {
        self == ExitInfo::Code(0)
    }
}

impl std::fmt::Display for ExitInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExitInfo::Code(c) => write!(f, "exit code {c}"),
            ExitInfo::Signal(s) => write!(f, "signal {s}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcOutcome {
    pub exit: ExitInfo,
    /// `ru_maxrss` of the reaped child, in KiB.
    pub peak_rss_kib: u64,
    pub wall: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("spawn {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("wait4: {0}")]
    Wait(std::io::Error),
    #[error("cancelled")]
    Cancelled(ProcOutcome),
}

/// Kills the process group if the run is abandoned before the child is reaped.
struct GroupGuard {
    pgid: libc::pid_t,
    armed: bool,
}

impl GroupGuard {
    fn kill(&self) {
        // SAFETY: killpg with a pgid we created; ESRCH (already gone) is harmless.
        unsafe {
            libc::killpg(self.pgid, libc::SIGKILL);
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.kill();
        }
    }
}

/// Spawn `cmd` and wait for it, killing its whole process group on `cancel`.
pub async fn run(
    cmd: &EngineCommand,
    cancel: &CancellationToken,
) -> Result<ProcOutcome, ProcError> {
    if cancel.is_cancelled() {
        return Err(ProcError::Cancelled(ProcOutcome {
            exit: ExitInfo::Signal(libc::SIGKILL),
            peak_rss_kib: 0,
            wall: Duration::ZERO,
        }));
    }
    let spawn_err = |source| ProcError::Spawn {
        program: cmd.program.display().to_string(),
        source,
    };
    let log = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&cmd.log)
        .map_err(spawn_err)?;
    let log_err = log.try_clone().map_err(spawn_err)?;

    let mut command = Command::new(&cmd.program);
    command
        .args(&cmd.args)
        .env_clear()
        .envs(cmd.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&cmd.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .process_group(0);
    let nice = cmd.nice;
    // SAFETY: the closure runs between fork and exec and only makes async-signal-safe
    // system calls.
    unsafe {
        command.pre_exec(move || {
            // Die with vcd, so an engine never outlives its supervisor.
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Everything the engine writes (output, logs, caches) is owner-only.
            libc::umask(0o077);
            // getpriority can return -1 as a valid value; then setting is still right.
            let current = libc::getpriority(libc::PRIO_PROCESS, 0);
            if nice > current && libc::setpriority(libc::PRIO_PROCESS, 0, nice) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let started = Instant::now();
    let child = command.spawn().map_err(spawn_err)?;
    let pid = child.id() as libc::pid_t;
    // The child is reaped by wait4 below; std's handle must not reap it.
    drop(child);
    let mut guard = GroupGuard {
        pgid: pid,
        armed: true,
    };

    let mut waiter = tokio::task::spawn_blocking(move || wait4(pid));
    let (result, cancelled) = tokio::select! {
        res = &mut waiter => (res, false),
        () = cancel.cancelled() => {
            guard.kill();
            (waiter.await, true)
        }
    };
    let (status, peak_rss_kib) = result
        .map_err(|e| ProcError::Wait(std::io::Error::other(e)))?
        .map_err(ProcError::Wait)?;
    // Reaped. Also kill anything the engine left behind in its group.
    guard.kill();
    guard.armed = false;

    let outcome = ProcOutcome {
        exit: status,
        peak_rss_kib,
        wall: started.elapsed(),
    };
    if cancelled {
        Err(ProcError::Cancelled(outcome))
    } else {
        Ok(outcome)
    }
}

fn wait4(pid: libc::pid_t) -> std::io::Result<(ExitInfo, u64)> {
    let mut status: libc::c_int = 0;
    // SAFETY: rusage is plain old data; zeroed is a valid value.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: valid pointers to locals; pid is our own child.
        let rc = unsafe { libc::wait4(pid, &mut status, 0, &mut usage) };
        if rc == pid {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
    let exit = if libc::WIFEXITED(status) {
        ExitInfo::Code(libc::WEXITSTATUS(status))
    } else {
        ExitInfo::Signal(libc::WTERMSIG(status))
    };
    Ok((exit, u64::try_from(usage.ru_maxrss).unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, dir: &std::path::Path) -> EngineCommand {
        EngineCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), script.into()],
            env: vec![("MARK".into(), "1".into())],
            cwd: dir.to_owned(),
            log: dir.join("log.txt"),
            nice: 0,
        }
    }

    #[tokio::test]
    async fn reports_exit_code_rss_and_captures_output() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = sh(
            "echo out; echo err >&2; echo \"$MARK-${HOME:-nohome}\"; touch made; exit 3",
            dir.path(),
        );
        let out = run(&cmd, &CancellationToken::new()).await.unwrap();
        assert_eq!(out.exit, ExitInfo::Code(3));
        assert!(out.peak_rss_kib > 0);
        let log = std::fs::read_to_string(dir.path().join("log.txt")).unwrap();
        assert!(log.contains("out") && log.contains("err"));
        // The environment is exactly what we passed.
        assert!(log.contains("1-nohome"), "{log}");
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.path().join("log.txt")), 0o600);
        // Files the child creates are owner-only too.
        assert_eq!(mode(&dir.path().join("made")), 0o600);
    }

    #[tokio::test]
    async fn reports_a_signal() {
        let dir = tempfile::tempdir().unwrap();
        let out = run(&sh("kill -SEGV $$", dir.path()), &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(out.exit, ExitInfo::Signal(libc::SIGSEGV));
    }

    #[tokio::test]
    async fn applies_nice() {
        let dir = tempfile::tempdir().unwrap();
        let read_nice = |cmd: EngineCommand| async move {
            run(&cmd, &CancellationToken::new()).await.unwrap();
            std::fs::read_to_string(&cmd.log)
                .unwrap()
                .trim()
                .parse::<i32>()
                .unwrap()
        };
        let script = "cut -d' ' -f19 /proc/self/stat";
        let base = read_nice(sh(script, dir.path())).await;
        let mut niced = sh(script, dir.path());
        niced.nice = 10;
        assert_eq!(read_nice(niced).await, base.max(10));
    }

    #[tokio::test]
    async fn cancel_kills_the_whole_group_and_reaps() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("started");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        // A grandchild and the shell both block forever once they announce themselves.
        let script = format!(
            "sleep 1000 & echo $! > {}/grandchild; echo go > {}; wait",
            dir.path().display(),
            fifo.display()
        );
        let cmd = sh(&script, dir.path());
        let cancel = CancellationToken::new();
        let task = {
            let cancel = cancel.clone();
            tokio::spawn(async move { run(&cmd, &cancel).await })
        };
        // Opening the FIFO for reading blocks until the shell has written to it.
        let fifo2 = fifo.clone();
        let msg = tokio::task::spawn_blocking(move || std::fs::read_to_string(fifo2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(msg.trim(), "go");
        cancel.cancel();
        let err = task.await.unwrap().unwrap_err();
        assert!(matches!(
            err,
            ProcError::Cancelled(ProcOutcome {
                exit: ExitInfo::Signal(libc::SIGKILL),
                ..
            })
        ));
        let gc: libc::pid_t = std::fs::read_to_string(dir.path().join("grandchild"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // The grandchild got SIGKILL with the group. A pidfd becomes readable when the
        // process terminates, so this waits for the event instead of polling.
        assert!(wait_for_exit(gc), "grandchild {gc} survived the group kill");
    }

    /// Block until `pid` terminates. False if the pidfd cannot be opened for a reason
    /// other than the process being gone already.
    fn wait_for_exit(pid: libc::pid_t) -> bool {
        // SAFETY: plain syscalls on our own fd.
        unsafe {
            let fd = libc::syscall(libc::SYS_pidfd_open, pid, 0) as libc::c_int;
            if fd < 0 {
                return std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
            }
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = libc::poll(&mut pfd, 1, -1);
            libc::close(fd);
            rc == 1
        }
    }

    #[tokio::test]
    async fn already_cancelled_does_not_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = run(&sh("touch spawned", dir.path()), &cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, ProcError::Cancelled(_)));
        assert!(!dir.path().join("spawned").exists());
    }

    #[tokio::test]
    async fn missing_program_is_a_spawn_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut cmd = sh("true", dir.path());
        cmd.program = dir.path().join("does-not-exist");
        assert!(matches!(
            run(&cmd, &CancellationToken::new()).await,
            Err(ProcError::Spawn { .. })
        ));
    }
}
