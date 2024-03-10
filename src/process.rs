use anyhow::{anyhow, Context};
use cancellation_token;
use cancellation_token::{CancelCallback, CancellationToken};
use nix::errno::Errno;
use nix::sys::signal;
use nix::unistd::Pid;
use std::error;
use std::fmt;
use std::os::unix::process::ExitStatusExt;
use std::process;

// TODO: This feels a bit like spooky action at a distance, maybe it woud be better to just have
// plain old functions for this. But it is quite nice to have the usage look fairly harmonic with
// normal process API usage. Since this is partly a learning project, I'll do the fancy thing and
// if it turns out to be too clever I'll have learned a lesson.
pub trait CommandExt {
    // Like std::process::Command::output, but SIGINTs the child if the token is canceled. Also
    // unconditionally captures stderr and stdout.
    fn output_ct(&mut self, ct: &CancellationToken) -> anyhow::Result<process::Output>;
    // Like the above, but fails if the process is terminated by a signal.
    fn output_not_killed(&mut self, ct: &CancellationToken) -> anyhow::Result<process::Output>;
    // Like the above, but also fails if the process exits with a non-zero return code.
    // This is a convenience hack, somewhat like
    // std::process::ExitStatus::exit_ok, but it's more informative. Arguably we
    // should have a ExitStatusExt for that rather than just squashing it into CommandExt.
    fn output_ok(&mut self, ct: &CancellationToken) -> anyhow::Result<()>;
}

// cancellation_token::Canceled doesn't implement std::errorr::Error so we can't put it into an
// anyhow error. Here's a custom error type that we can.
//
// TODO: Maybe it's dumb to squash cancellation status into the error band. Should we nest
// MayBeCanceled with Result, or just return an Option which is None under cancellation? That would
// be pretty un-ergonomic in cases where we don't expect cancellation though.=
#[derive(Debug)]
pub struct Canceled {}

impl fmt::Display for Canceled {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Write strictly the first element into the supplied output
        // stream: `f`. Returns `fmt::Result` which indicates whether the
        // operation succeeded or failed. Note that `write!` uses syntax which
        // is very similar to `println!`.
        write!(f, "canceled")
    }
}

impl error::Error for Canceled {
    // If we integrated properly with the cancellation library it would be cool to report the token
    // hierarchy here.
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        None
    }
    fn description(&self) -> &str {
        "token canceled"
    }
    fn cause(&self) -> Option<&dyn error::Error> {
        None
    }
}

impl CommandExt for process::Command {
    fn output_ct(&mut self, ct: &CancellationToken) -> anyhow::Result<process::Output> {
        self.stderr(process::Stdio::piped());
        self.stdout(process::Stdio::piped());
        let child = self.spawn().context("spawning child process")?;

        let pid = Pid::from_raw(child.id() as i32);
        let _ct_reg = ct.register(CancelCallback::FnOnce(Box::new(move || {
            match signal::kill(pid, signal::SIGINT) {
                Ok(_) => (),
                Err(Errno::ESRCH) => (), // The process probably just terminated.
                // TODO logging to a sensible place?
                Err(errno) => println!("Couldn't kill pid {}: {}", pid, errno.desc()),
            }
        })));

        let output = child.wait_with_output().context("awaiting child")?;
        if ct.is_canceled() {
            return Err(anyhow::Error::new(Canceled {}));
        }
        return Ok(output);
    }

    fn output_not_killed(&mut self, ct: &CancellationToken) -> anyhow::Result<process::Output> {
        let output = self.output_ct(ct)?;
        match output.status.code() {
            None => Err(anyhow!(
                "terminated by signal {}",
                output
                    .status
                    .signal()
                    .expect("ExitStatus::code() and ExitStatus::signal() both None")
            )),
            Some(_) => Ok(output),
        }
    }

    fn output_ok(&mut self, ct: &CancellationToken) -> anyhow::Result<()> {
        let output = self.output_not_killed(ct)?;
        match output.status.code().unwrap() {
            0 => Ok(()),
            code => Err(anyhow!(
                "failed with exit code {}. stderr:\n{}\nstdout:\n{}",
                code,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            )),
        }
    }
}
