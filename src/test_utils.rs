use std::{path::Path, time::Duration};

use anyhow::bail;
use chrono::{DateTime, Utc};
use futures::Future;
use tokio::{select, time::{interval, sleep}};

pub async fn timeout_5s<F, T>(fut: F) -> anyhow::Result<T>
where
    F: Future<Output = T>,
{
    select!(
        _ = sleep(Duration::from_secs(5)) => bail!("timeout after 5s"),
        output = fut => Ok(output)
    )
}

// Blocks until file exists, the dumb way.
pub async fn path_exists<P>(path: P)
where
    P: AsRef<Path>,
{
    let mut interval = interval(Duration::from_millis(10));
    while !path.as_ref().try_exists().unwrap() {
        interval.tick().await;
    }
}

pub fn some_time() -> DateTime<Utc> {
    "2012-12-12T12:12:12Z".parse().unwrap()
}

pub mod git {
    use std::ffi::OsStr;

    use anyhow::{anyhow, Context as _};
    use chrono::{DateTime, Utc};
    use tempfile::TempDir;

    use crate::{git::{CommitHash, Worktree}, process::{CommandExt as _, OutputExt as _}};

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
        #[allow(async_fn_in_trait)] // Only used within this project
        async fn commit<S>(&self, message: S, timestamp: DateTime<Utc>) -> anyhow::Result<CommitHash>
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
        #[allow(async_fn_in_trait)] // Only used within this project
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
            Ok(Some(CommitHash::new(out_string.trim().to_owned())))
        }
    }

    impl<W: Worktree> WorktreeExt for W { }
}
