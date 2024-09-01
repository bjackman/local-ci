use std::{
    fs, io::Write, ops::{Deref, DerefMut}, os::unix::process::ExitStatusExt, path::Path, process::{Child, Stdio}, str::FromStr, time::{Duration, Instant}
};

use anyhow::{bail, Context as _, Result};
use glob::glob;
use log::error;
use nix::{
    libc::pid_t,
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use tempfile::TempDir;
use test_bin::get_test_bin;
use test_case::test_case;
use test_log;

fn wait_for<F>(mut predicate: F, timeout: Duration) -> anyhow::Result<()>
where
    F: FnMut() -> anyhow::Result<bool>,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if predicate().context("timeout predicate failed")? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("timeout after {:?}", timeout)
}

struct ChildKilledOnDrop(Child);

impl Drop for ChildKilledOnDrop {
    fn drop(&mut self) {
        let _ = self
            .0
            .kill()
            .map_err(|e| error!("Couldn't kill child on shutdown: {}", e));
    }
}

impl Deref for ChildKilledOnDrop {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.0
    }
}

impl DerefMut for ChildKilledOnDrop {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

// Add a reasonable method for sending general signals, Rust only provides a method to SIGKILL.
pub trait ChildExt {
    fn signal(&self, sig: Signal) -> anyhow::Result<()>;
}

impl ChildExt for Child {
    fn signal(&self, sig: Signal) -> anyhow::Result<()> {
        let pid = Pid::from_raw(self.id().try_into().context("couldnt parse child PID")?);
        kill(pid, sig).context("couldn't signal child")
    }
}

// An instance of the binary, running as a child process.
struct LocalCiChild {
    temp_dir: TempDir,
    child: ChildKilledOnDrop,
}

impl LocalCiChild {
    fn new(config: String) -> Result<Self> {
        let temp_dir = TempDir::new()?;
        let mut cmd = get_test_bin("local-ci");
        // TODO: This uses this code's repo as a test input, so maybe we can break
        // this test by just commiting changes. Should probably have a special test
        // repo as input.
        let cmd = cmd
            .args([
                "HEAD^",
                "--config",
                "/dev/stdin",
                "--worktree-dir",
                temp_dir.path().to_str().unwrap(),
                "--worktree-prefix",
                "test-worktree-",
            ])
            .stdin(Stdio::piped());
        let mut child = ChildKilledOnDrop(cmd.spawn().unwrap());
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(config.as_bytes()).unwrap();
        Ok(Self { temp_dir, child })
    }

    // Returns true if any worktree of this child currently exists.
    fn has_worktrees(&mut self) -> Result<bool> {
        let mut pattern = self.temp_dir.path().to_owned();
        pattern.push("test-worktree-*");
        Ok(!glob(pattern.to_string_lossy().as_ref())?
            .collect::<Vec<_>>()
            .is_empty())
    }

    fn terminate(&mut self) -> Result<()> {
        self.child.signal(Signal::SIGINT).unwrap();
        wait_for(
            || {
                let exit_status = self
                    .child
                    .try_wait()
                    .context("couldn't check child status")?;
                match exit_status {
                    None => Ok(false), // Still running
                    Some(exit_status) => {
                        if exit_status.success() {
                            Ok(true)
                        } else {
                            bail!(
                                "test binary failed ({exit_status:?} exit code {:?} exit signal {:?}",
                                exit_status.code(),
                                exit_status.signal()
                            )
                        }
                    }
                }
            },
            Duration::from_secs(5),
        )
    }
}

#[test_case("echo hello world"; "clean worktree")]
#[test_case("echo hello world > file.txt"; "dirty worktree")]
#[test_case("echo hello world > file.txt && git add file.txt"; "really dirty worktree")]
#[test_log::test]
fn test_worktree_teardown(test_command: &str) {
    let mut lci = LocalCiChild::new(format!(
        r##"
        num_worktrees = 1
        [[tests]]
        name = "my_test"
        command = {test_command:?}
    "##
    ))
    .unwrap();

    wait_for(|| lci.has_worktrees(), Duration::from_secs(5))
        .expect(&format!("worktree not found after 5s"));

    lci.terminate().expect("couldn't shut down child");

    assert!(
        !lci.has_worktrees().unwrap(),
        "worktrees not cleaned up on SIGINT"
    );
}

fn pid_running(pid: pid_t) -> bool {
    return Path::new(&format!("/proc/{pid}")).exists()
}

#[test_log::test]
fn shouldnt_leak_jobs() {
    let temp_dir = TempDir::new().unwrap();

    // This config has a test that does not respect SIGINT. We should not leak
    // that job.
    let mut lci = LocalCiChild::new(format!(
        r##"
        num_worktrees = 1
        [[tests]]
        name = "my_test"
        command = "echo $$ > {}/test_pid; while true; do sleep infinity; done"
        shutdown_grace_period_s = 1
    "##,
        temp_dir.path().to_string_lossy()
    ))
    .unwrap();

    // Wait for test to start up
    let test_pid_path = temp_dir.path().join("test_pid");
    wait_for(|| Ok(test_pid_path.exists()), Duration::from_secs(5))
        .expect(&format!("worktree not found after 5s"));
    let pid: pid_t = pid_t::from_str(&fs::read_to_string(test_pid_path).unwrap().trim()).unwrap();

    lci.terminate().unwrap();
    assert!(!pid_running(pid));
}
