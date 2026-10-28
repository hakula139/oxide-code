//! The MSYS runtime that Git Bash shells share on Windows, kept set up by an idle bash.

use std::io::{self, Read as _};
use std::path::Path;
use std::process::{Child, Command, Stdio};
#[cfg(windows)]
use std::time::Duration;

use tokio::sync::{Mutex, oneshot};
use tracing::warn;

use crate::config::auth::withhold_secrets;

/// The runtime of the bash every shell starts with.
#[cfg(windows)]
static RUNTIME: MsysRuntime = MsysRuntime::new();

/// How long a shell waits for the holding bash to start, which is how long an MSYS process waits for
/// another to set up the runtime.
#[cfg(windows)]
const START_LIMIT: Duration = Duration::from_secs(15);

/// Holds the runtime of `bash`, waiting up to [`START_LIMIT`] for the holder to start.
#[cfg(windows)]
pub(super) async fn hold_runtime(bash: &Path) {
    RUNTIME
        .hold(
            || MsysRuntime::holder(bash),
            tokio::time::sleep(START_LIMIT),
        )
        .await;
}

/// An idle bash that keeps the per-user shared memory of the MSYS runtime set up while ox runs.
///
/// The first MSYS process to map that memory builds its mount table under a lock that it releases
/// when it finishes. If it dies first, as a shell killed during startup does, every MSYS process
/// started after it waits 15 s and aborts with `add_item (...) failed, errno 1` until no process
/// maps the memory (git-for-windows/git#6368). A process that starts once the memory is set up
/// skips the lock, so ox holds the runtime before it starts any shell that it may kill.
struct MsysRuntime(Mutex<Option<Holder>>);

impl MsysRuntime {
    const fn new() -> Self {
        Self(Mutex::const_new(None))
    }

    /// The bash that holds the runtime. It prints a line once its runtime is up and exits once its
    /// stdin closes, as it does when ox exits.
    fn holder(bash: &Path) -> Command {
        let mut cmd = Command::new(bash);
        cmd.args(["-c", "echo; read"]);
        withhold_secrets(&mut cmd);
        cmd
    }

    /// Starts `holder` unless a holder runs, then waits for its runtime to be up until `deadline`
    /// completes. A holder still starting when the wait ends stays for the next hold to wait on.
    async fn hold(&self, holder: impl FnOnce() -> Command, deadline: impl Future<Output = ()>) {
        let mut slot = self.0.lock().await;
        if !slot.as_mut().is_some_and(Holder::running) {
            *slot = Holder::spawn(holder())
                .inspect_err(|e| warn!(error = %e, "failed to start the MSYS runtime holder"))
                .ok();
        }
        let Some(holder) = slot.as_mut() else {
            return;
        };
        let Some(line) = &mut holder.line else {
            return;
        };

        let started = tokio::select! {
            biased;
            started = line => started.expect("the reader sends its result"),
            () = deadline => {
                warn!("the MSYS runtime holder is still starting");
                return;
            }
        };
        match started {
            Ok(()) => holder.line = None,
            Err(e) => {
                warn!(error = %e, "failed to start the MSYS runtime holder");
                *slot = None;
            }
        }
    }
}

/// A bash holding the runtime, with the wait for its line until the line arrives.
struct Holder {
    child: Child,
    line: Option<oneshot::Receiver<io::Result<()>>>,
}

impl Holder {
    /// Spawns `cmd` with its stdin open and a thread waiting for the first byte of its stdout.
    fn spawn(mut cmd: Command) -> io::Result<Self> {
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdout = child.stdout.take().expect("stdout configured as piped");
        let (sender, line) = oneshot::channel();
        std::thread::spawn(move || {
            let started = match stdout.read(&mut [0]) {
                Ok(0) => Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "bash exited before printing its line",
                )),
                read => read.map(drop),
            };
            _ = sender.send(started);
        });
        Ok(Self {
            child,
            line: Some(line),
        })
    }

    fn running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::ffi::OsStr;
    use std::future::{pending, ready};
    use std::io::Write as _;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::config::auth::secret_env_vars;
    use crate::tool::bash::bash;

    /// A bash that runs `script`.
    fn bash_running(script: &str) -> Command {
        let mut cmd = Command::new(bash().unwrap());
        cmd.args(["-c", script]);
        cmd
    }

    /// The process ID of the holder while it runs, and whether its runtime is up.
    fn running_holder(runtime: &MsysRuntime) -> Option<(u32, bool)> {
        let mut slot = runtime.0.try_lock().unwrap();
        let holder = slot.as_mut()?;
        holder
            .running()
            .then(|| (holder.child.id(), holder.line.is_none()))
    }

    mod holder {
        use super::*;

        #[test]
        fn withholds_the_secrets_ox_authenticates_with() {
            let holder = MsysRuntime::holder(Path::new("bash"));

            let removed: BTreeSet<_> = holder
                .get_envs()
                .filter_map(|(var, value)| value.is_none().then_some(var))
                .collect();
            assert_eq!(removed, secret_env_vars().map(OsStr::new).collect());
        }
    }

    mod hold {
        use super::*;

        #[tokio::test]
        async fn keeps_one_bash_until_its_stdin_closes_then_starts_another() {
            let runtime = MsysRuntime::new();
            let holder = || MsysRuntime::holder(bash().unwrap());

            runtime.hold(holder, pending()).await;

            let first = running_holder(&runtime);
            assert!(first.is_some_and(|(_, up)| up), "no bash holds the runtime");

            runtime
                .hold(|| unreachable!("a holder runs"), pending())
                .await;

            assert_eq!(running_holder(&runtime), first);

            let mut slot = runtime.0.try_lock().unwrap();
            drop(slot.as_mut().unwrap().child.stdin.take());
            drop(slot);
            let deadline = Instant::now() + Duration::from_secs(10);
            while running_holder(&runtime).is_some() {
                assert!(Instant::now() < deadline, "the holder outlived its stdin");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            runtime.hold(holder, pending()).await;

            assert!(
                running_holder(&runtime).is_some_and(|(_, up)| up),
                "no bash replaced the holder"
            );
        }

        #[tokio::test]
        async fn waits_again_for_a_bash_still_starting_at_the_deadline() {
            let runtime = MsysRuntime::new();

            runtime
                .hold(|| bash_running("read && echo && read"), ready(()))
                .await;

            let late = running_holder(&runtime);
            assert!(late.is_some_and(|(_, up)| !up), "no bash still starting");

            let mut slot = runtime.0.try_lock().unwrap();
            let stdin = slot.as_mut().unwrap().child.stdin.as_mut().unwrap();
            stdin.write_all(b"\n").unwrap();
            drop(slot);
            runtime
                .hold(|| unreachable!("the late holder runs"), pending())
                .await;

            assert_eq!(
                running_holder(&runtime),
                late.map(|(id, _)| (id, true)),
                "the late holder did not start",
            );
        }

        #[tokio::test]
        async fn starts_another_bash_once_one_fails_to_start() {
            let cases = [
                ("exit", bash_running("exit 3")),
                ("missing bash", Command::new("/nonexistent/bash")),
            ];
            for (case, failing) in cases {
                let runtime = MsysRuntime::new();

                runtime.hold(|| failing, pending()).await;

                let kept = runtime.0.try_lock().unwrap().is_some();
                assert!(!kept, "{case}: kept the failed holder");

                runtime
                    .hold(|| MsysRuntime::holder(bash().unwrap()), pending())
                    .await;

                assert!(
                    running_holder(&runtime).is_some_and(|(_, up)| up),
                    "{case}: no bash replaced the failed holder"
                );
            }
        }
    }
}
