use anyhow::Context;
use clap::Parser as _;
use futures::StreamExt;
use std::ffi::OsString;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::{env, str};
use tokio::{select, signal};

use crate::git::Worktree;

mod config;
mod git;
mod process;
mod resource;
mod status;
mod test;

#[cfg(test)]
mod test_utils;

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    // TODO: Don't require valid utf-8 strings here, OsStrings shoud be fine. But
    // https://stackoverflow.com/questions/76341332/clap-default-value-for-pathbuf
    #[arg(short, long, default_value_t = {".".to_string()})]
    repo: String,
    /// Maximum number of tests to run concurrently. Each concurrent thread
    /// requires creating a worktree, which is why we don't default to $nproc.
    #[arg(short, long, default_value_t = 8)]
    num_threads: u32,
    /// Filename prefix for temporary worktrees.
    #[arg(long, default_value_t = {"local-ci-worktree".to_string()})]
    worktree_prefix: String,
    /// Directory (must exist) to create temporary worktrees in.
    #[arg(long, default_value_t = {env::temp_dir().to_string_lossy().into_owned()})]
    worktree_dir: String,
    /// Command to test. Note this is _not_ run via the shell.
    #[arg(short, long, required = true)]
    config: PathBuf,
    /// Base of range to test. Will test commits between this (exclusive) and
    /// HEAD (inclusive). Whenever HEAD changes, this string will be re-evaluated
    /// to find the base of the range.
    base: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    env_logger::init();

    let repo = git::PersistentWorktree {
        path: args.repo.to_owned().into(),
    };
    // Check repo is valid.
    repo.git_dir()
        .await
        .context(format!("opening repo {}", args.repo))?;
    let repo = Arc::new(repo);
    let manager_builder = config::manager_builder(repo.clone(), &args.config)?
        .worktree_prefix(&args.worktree_prefix)
        .worktree_dir(&args.worktree_dir);
    let mut m = manager_builder.build().await?;
    let range_spec: OsString = format!("{}..HEAD", args.base).into();
    let mut result_tracker = status::Tracker::new();
    let mut results = m.results();
    m.set_revisions(
        repo.rev_list(&range_spec)
            .await
            .context("couldn't rev-list")?,
    );
    let mut revs_stream = repo.watch_refs(&range_spec)?;
    let mut revs_stream = pin!(revs_stream);
    loop {
        select!(
            // TODO: It's dumb that we have two different types of communication here (one exposes
            // the channel, one implements Stream).
            revs = revs_stream.next() => {
                // TODO: figure out if/how this can actually fail.
                let revs = revs.expect("revset stream terminated")?;
                result_tracker.set_revisions(revs.clone());
                m.set_revisions(revs);
            },
            result = results.recv() => {
                // https://github.com/rust-lang/futures-rs/issues/1857
                // AFAICS there is no way to encode a stream that never terminates.
                let result = result.expect("result stream terminated");
                result_tracker.update(result.clone());
                println!("{}", result);
            },
            _ =  signal::ctrl_c() => break,
        )
    }
    m.settled().await;
    Ok(())
}
