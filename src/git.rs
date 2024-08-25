use core::fmt;
use core::fmt::{Debug, Display};
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process::Command as SyncCommand;
use std::str;
use std::time::Duration;

#[cfg(test)]
use anyhow::anyhow;
use anyhow::{bail, Context};
use async_stream::try_stream;
use futures::{future::Fuse, select, FutureExt, SinkExt as _, StreamExt as _};
use futures_core::{stream::Stream, FusedFuture};
use log::{debug, error, info};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;

use crate::process::CommandExt;
use crate::process::{OutputExt, SyncCommandExt};

#[derive(Clone, PartialEq, Eq, Debug, Hash)]
pub struct CommitHash(pub String);

impl AsRef<OsStr> for CommitHash {
    fn as_ref(&self) -> &OsStr {
        OsStr::from_bytes(self.0.as_bytes())
    }
}

impl Display for CommitHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// Worktree represents a git tree, which might be the "main" worktree (in which case it might be
// more clearly refrred to by the name Repo) or some other one.
#[derive(Debug)]
pub struct PersistentWorktree {
    pub path: PathBuf,
}

impl Worktree for PersistentWorktree {
    fn path(&self) -> &Path {
        &self.path
    }
}

// This is a weird kinda inheritance type thing to enable different types of worktree (with
// different fields and drop behaviours) to share the functionality that users actually care about.
// Not really sure if this is the Rust Way or not.
pub trait Worktree: Debug {
    fn path(&self) -> &Path;

    // Convenience function to create a git command with some pre-filled args.
    fn git<I, S>(&self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = Command::new("git");
        cmd.current_dir(self.path()).args(args);
        cmd
    }

    async fn git_dir(&self) -> anyhow::Result<PathBuf> {
        let output = self
            .git(["rev-parse", "--git-dir"])
            .execute()
            .await
            .context("'git rev-parse --git-dir' failed")?;
        Ok(OsStr::from_bytes(&output.stderr).into())
    }

    async fn rev_list<S>(&self, range_spec: S) -> anyhow::Result<Vec<CommitHash>>
    where
        S: AsRef<OsStr>,
    {
        let output = self
            .git(["rev-list"])
            .arg(range_spec)
            .execute()
            .await
            .context("'git rev-list' failed")?;
        // See coment in rev_parse.
        if output.code_not_killed()? == 128 {
            return Ok(vec![]);
        }
        let code = output.status.code().unwrap();
        if code != 0 {
            bail!(
                "failed with exit code {}. stderr:\n{}\nstdout:\n{}",
                code,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
        }
        let out_str: &str = str::from_utf8(&output.stdout).context("non utf-8 rev-list output")?;
        Ok(out_str.lines().map(|l| CommitHash(l.to_owned())).collect())
    }

    async fn checkout(&self, commit: &CommitHash) -> anyhow::Result<()> {
        self.git(["checkout"])
            .arg(commit)
            .output()
            .await?
            .ok()
            .context(format!(
                "checking out revision {:?} in {:?}",
                commit,
                self.path()
            ))
    }

    async fn log_graph<S, T>(&self, range_spec: S, format_spec: T) -> anyhow::Result<OsString>
    where
        S: AsRef<OsStr>,
        T: AsRef<OsStr>,
    {
        let mut format_arg = OsString::from("--format=");
        format_arg.push(format_spec.as_ref());
        let stdout = self
            .git(["log", "--graph", "--color=always"])
            .args([&format_arg, range_spec.as_ref()])
            .execute()
            .await
            .context(format!(
                "getting graph log for {:?} with format {:?}",
                range_spec.as_ref(),
                format_spec.as_ref(),
            ))?
            .stdout;
        Ok(OsString::from_vec(stdout))
    }

    async fn log_n1<S, T>(&self, rev_spec: S, format_spec: T) -> anyhow::Result<OsString>
    where
        S: AsRef<OsStr>,
        T: AsRef<OsStr>,
    {
        let mut format_arg = OsString::from("--format=");
        format_arg.push(format_spec.as_ref());
        let stdout = self
            .git(["log", "-n1", "--color=always"])
            .args([&format_arg, rev_spec.as_ref()])
            .execute()
            .await
            .context(format!(
                "getting -n1 log for {:?} with format {:?}",
                rev_spec.as_ref(),
                format_spec.as_ref(),
            ))?
            .stdout;
        Ok(OsString::from_vec(stdout))
    }

    // Watch for events that could change the meaning of a revspec. When that happens, send an event
    // on the channel with the new resolved spec.
    fn watch_refs<'a>(
        &'a self,
        // TODO: Write this in a way where the user doesn't have to deal with converting to OsStr.
        // (Needs to also work with both owned and reference types I think).
        range_spec: &'a OsStr,
    ) -> anyhow::Result<impl Stream<Item = anyhow::Result<Vec<CommitHash>>> + 'a> {
        // Alternatives considered/attempted:
        //
        // - inotify (also fanotify) doesn't support recursively watching directories, whereas the
        //   notify crate has convenient support for that.
        // - The notify crate has convenient support for sending stuff directly down std::sync::mpsc
        //   channels, and even has support for debouncing those events. However it seems like you
        //   then need to create a whole additional channel if you wanna "map" the events to
        //   something else, i.e. like we wanna call rev_list here.
        //
        // Overall the idea of how to turn this into an async thingy comes from
        // https://github.com/notify-rs/notify/blob/main/examples/async_monitor.rs, I am not sure if
        // this is "real" or toy code that I should not have followed so literally. I do think that
        // this use of futures::executor::block_on is legit - the notify crate spins up a thread
        // under the hood so it's fine to block that thread, and block_on seems to be the proper way
        // to bridge into async code from sync code.
        let (mut tx, mut rx) = futures::channel::mpsc::unbounded();

        let mut watcher = RecommendedWatcher::new(
            move |res| {
                futures::executor::block_on(async {
                    // The documentation is very confusing here, it's hard to figure out why send
                    // would fail. To be my best understanding it just means that the receiver has
                    // been dropped. It's extremely non-obvious whether we can expect this to happen
                    // here. The receiver was declared before the watcher, so the watcher should be
                    // dropped first, right? But, then presumably we move both of them into the
                    // stream object. So, which one gets dropped first? No fucking idea. We'll just
                    // log if an error occurs and maybe it will be helpful for debugging something
                    // else.
                    tx.send(res).await.unwrap_or_else(|err| {
                        info!(
                            "error in git watcher internal send (probably harmless if shutting down): {}",
                            err
                        )
                    });
                })
            },
            Config::default(),
        )?;
        watcher
            .watch(self.path(), RecursiveMode::Recursive)
            .context("setting up watcher")?;

        // This logic "debounces" consecutive events within the same 1s window, to avoid thrashing
        // on the downstream logic as Git works its way through changes.
        Ok(try_stream! {
            let _watcher = watcher; // Capture so it doesn't get dropped

            // Produce an initial update.
            yield self.rev_list(range_spec).await?;

            // Start with an expired timer.
            let mut sleep_fut = pin!(Fuse::terminated());
            loop {
                select! {
                    // Produce an update when the timer expires.
                    () = sleep_fut =>  yield self.rev_list(range_spec).await?,
                    // Ensure the timer is set when we see an update.
                    result = rx.next() => {
                        // There's a bug if the sender has shut down, we should always receive
                        // something.
                        let _ = result.expect("git watcher internal receive error");
                        if sleep_fut.is_terminated() {
                            sleep_fut.set(sleep(Duration::from_secs(1)).fuse());
                        }
                    },
                }
            }
        })
    }
}

// A worktree that is deleted when dropped. This is kind of a dumb API that just happens to fit this
// project's exact needs. Instead probably Repo::new and this method should return a common trait or
// something.
#[derive(Debug)]
pub struct TempWorktree {
    origin: PathBuf, // Path of repo this was created from.
    temp_dir: TempDir,
}

impl TempWorktree {
    // Create a worktree based on the origin repo, directly in the temp dir (which should be empty)
    pub async fn new<W>(origin: &W, temp_dir: TempDir) -> anyhow::Result<TempWorktree>
    where
        W: Worktree,
    {
        origin
            .git(["worktree", "add"])
            .arg(temp_dir.path())
            .arg("HEAD")
            .execute()
            .await
            .context("'git worktree add' failed")?;

        Ok(Self {
            origin: origin.path().to_owned(),
            temp_dir,
        })
    }
}

impl Worktree for TempWorktree {
    fn path(&self) -> &Path {
        self.temp_dir.path()
    }
}

impl Drop for TempWorktree {
    fn drop(&mut self) {
        let mut cmd = SyncCommand::new("git");
        if !self.origin.exists() {
            debug!(
                "Not de-registering worktree at {:?} as origin repo ({:?}) is gone.",
                self.temp_dir.path(),
                self.origin
            );
            return;
        }
        cmd.args(["worktree", "remove", "--force"])
            .arg(self.temp_dir.path())
            .current_dir(&self.origin)
            .execute()
            .unwrap_or_else(|e| {
                error!("Couldn't clean up worktree {:?}: {:?}", &self.temp_dir, e);
            });
        debug!("Delorted worktree at {:?}", self.temp_dir.path());
    }
}

#[cfg(test)]
pub mod test_utils {

    use chrono::{DateTime, Utc};

    use super::*;

    #[derive(Debug)]
    pub struct TempRepo {
        temp_dir: TempDir,
    }

    // Empty repository in a temporary directory, torn down on drop.
    impl TempRepo {
        pub async fn new() -> anyhow::Result<Self> {
            // https://www.youtube.com/watch?v=_MwboA5NIVA
            let zelf = Self {
                temp_dir: TempDir::with_prefix("fixture-").expect("couldn't make tempdir"),
            };
            zelf.git(["init"]).execute().await?;
            Ok(zelf)
        }
    }

    impl Worktree for TempRepo {
        fn path(&self) -> &Path {
            self.temp_dir.path()
        }
    }

    pub trait WorktreeExt: Worktree {
        // timestamp is used for both committer and author. This ought to make
        // commit hashes deterministic.
        async fn commit<S>(
            &self,
            message: S,
            timestamp: DateTime<Utc>,
        ) -> anyhow::Result<CommitHash>
        where
            S: AsRef<OsStr>,
        {
            let ts_is08601 = format!("{}", timestamp.format("%+"));
            self.git(["commit", "-m"])
                .arg(message)
                .arg("--allow-empty")
                .env("GIT_AUTHOR_DATE", ts_is08601.clone())
                .env("GIT_COMMITTER_DATE", ts_is08601)
                .execute()
                .await
                .context("'git commit' failed")?;
            // Doesn't seem like there's a safer way to do this than commit and then retroactively parse
            // HEAD and hope nobody else is messing with us.
            self.rev_parse("HEAD")
                .await?
                .ok_or(anyhow!("no HEAD after committing"))
        }

        // None means we successfully looked it up but it didn't exist.
        async fn rev_parse<S>(&self, rev_spec: S) -> anyhow::Result<Option<CommitHash>>
        where
            S: AsRef<OsStr>,
        {
            let output = self
                .git(["rev-parse"])
                .arg(rev_spec)
                .execute()
                .await
                .context("'git rev-parse' failed")?;
            // Hack: empirically, rev-parse returns 128 when the range is invalid, it's not documented
            // but hopefully this is stable behaviour that we're supposed to be able to rely on for
            // this...?
            if output.code_not_killed()? == 128 {
                return Ok(None);
            }
            let out_string =
                String::from_utf8(output.stdout).context("reading git rev-parse output")?;
            Ok(Some(CommitHash(out_string.trim().to_owned())))
        }

        async fn merge(
            &self,
            parents: &[CommitHash],
            timestamp: DateTime<Utc>,
        ) -> anyhow::Result<()> {
            let ts_is08601 = format!("{}", timestamp.format("%+"));
            self.git(["merge", "-m", "merge commit"])
                .args(parents)
                .env("GIT_AUTHOR_DATE", ts_is08601.clone())
                .env("GIT_COMMITTER_DATE", ts_is08601)
                .execute()
                .await
                .context("'git commit' failed")?;
            Ok(())
        }
    }

    impl<W: Worktree> WorktreeExt for W {}
}

#[cfg(test)]
mod tests {
    use std::fs::File;
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;

    #[test_log::test(tokio::test)]
    async fn test_new_gitdir_notgit() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        let wt = PersistentWorktree {
            path: tmp_dir.path().to_path_buf(),
        };
        assert!(
            wt.git_dir().await.is_err(),
            "opening repo with no .git didn't fail"
        );
    }

    #[test_log::test(tokio::test)]
    async fn test_new_gitdir_file_notgit() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        {
            let mut bogus_git_file =
                File::create(tmp_dir.path().join(".git")).expect("couldn't create .git");
            write!(bogus_git_file, "no no no").expect("couldn't write .git");
        }
        let wt = PersistentWorktree {
            path: tmp_dir.path().to_path_buf(),
        };
        assert!(
            wt.git_dir().await.is_err(),
            "opening repo with bogus .git file didn't fail"
        );
    }
}
