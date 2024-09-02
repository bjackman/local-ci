use anyhow::Context;
use clap::Parser as _;
use futures::StreamExt;
use log::info;
use tokio_util::sync::CancellationToken;
use std::ffi::OsString;
use std::io::stdout;
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
mod result;

#[cfg(test)]
mod test_utils;

// You can't import code from your own crate if you have your integration tests
// in a separate crate. But we want to use our git and process utilitities in
// the tests, so we just treat them as normal unit tests.
#[cfg(test)]
mod integration_test;

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

    // Set up shutdown first, to ensure we correctly handle early signals.
    // As well as doing it early, it seems to be important that we do this with
    // a single global signal::ctrl_c call, if I call this in the select loop I
    // would occasionally observe that SIGINT kills the program instead of
    // triggering Tokio's signal handler, this is because we require the ctrl_c
    // future to get polled before we do any work, since that's where it
    // installs the signal handler.
    // TOOD: this still seems racy though, because in theory we could get all
    // the way into the setup below before the task here ever gets to polling
    // the future. It seems like this just means the ctrl_c design is bad and we
    // should probably just not use it.
    let cancellation_token = CancellationToken::new();
    let token = cancellation_token.clone();
    tokio::spawn(async move {
        signal::ctrl_c().await.expect("error listening for ctrl-C");
        token.cancel()
    });

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
    let mut test_manager = manager_builder.build().await?;
    let range_spec: OsString = format!("{}..HEAD", args.base).into();
    let mut notifs = test_manager.results();
    let mut status_tracker = status::Tracker::new(repo.clone(), stdout());
    let mut revs_stream = repo.watch_refs(&range_spec)?;
    let mut revs_stream = pin!(revs_stream);
    loop {
        select!(
            // TODO: It's dumb that we have two different types of communication here (one exposes
            // the channel, one implements Stream).
            revs = revs_stream.next() => {
                // TODO: figure out if/how this can actually fail.
                let revs = revs.expect("revset stream terminated")?;
                // Paying for a pointless clone here so we can do set_revisions
                // (mostly just kicks off background stuff) before awaiting the
                // status tracker reset (does synchronhous work).
                test_manager.set_revisions(revs.clone())?;
                status_tracker.set_range(&range_spec).await.context("resetting status tracker")?;
                status_tracker.repaint().context("error painting status to stdout")?;
            },
            notif = notifs.recv() => {
                // https://github.com/rust-lang/futures-rs/issues/1857
                // AFAICS there is no way to encode a stream that never terminates.
                let notif = notif.expect("notification stream terminated");
                status_tracker.update(notif);
                status_tracker.repaint().context("error painting status to stdout")?;
            },
            _ =  cancellation_token.cancelled() => {
                info!("Got shutdown signal, terminating jobs and waiting");
                test_manager.set_revisions([])?;
                break;
            }
        )
    }
    test_manager.settled().await;
    Ok(())
}
