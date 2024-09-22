use anyhow::Context;
use clap::{arg, value_parser, Arg, ArgMatches, FromArgMatches, Parser as _};
use futures::StreamExt;
use log::info;
use std::ffi::OsString;
use std::io::stdout;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::{env, str};
use tokio::{select, signal};
use tokio_util::sync::CancellationToken;

use crate::git::Worktree;

mod config;
mod git;
mod http;
mod process;
mod resource;
mod result;
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
    #[command(flatten)]
    runtime_default: RuntimeDefaultArgs,
    /// Base of range to test. Will test commits between this (exclusive) and
    /// HEAD (inclusive). Whenever HEAD changes, this string will be re-evaluated
    /// to find the base of the range.
    base: String,
}

// Clap's derive API doesn't support argument defaults that depend on runtime
// computation. We don't wanna drop that API completely so
// we use the tricks from
// https://docs.rs/clap/latest/clap/_derive/index.html#flattening-hand-implemented-args-into-a-derived-application
// to combine the derive and builder APIs.
// This builder-API based part is absolutely fucking insane,
// I think there's very likely a less totally fucking insane way to write it
// but this is too tedious, I can't be bothered to figure it out.
struct RuntimeDefaultArgs {
    /// Directory where results will be stored.
    result_db: PathBuf,
}

// IIUC this diabolical bullshit is to take the parsed arguments ant put thme
// into theo RuntimeDefaultArgs struct that we flatten into the derive'd struct
// above.
impl FromArgMatches for RuntimeDefaultArgs {
    fn from_arg_matches(matches: &ArgMatches) -> Result<Self, clap::Error> {
        let mut matches = matches.clone();
        Self::from_arg_matches_mut(&mut matches)
    }
    fn from_arg_matches_mut(matches: &mut ArgMatches) -> Result<Self, clap::Error> {
        Ok(Self {
            result_db: matches.get_one::<PathBuf>("result-db").unwrap().to_owned(),
        })
    }
    fn update_from_arg_matches(&mut self, matches: &ArgMatches) -> Result<(), clap::Error> {
        let mut matches = matches.clone();
        self.update_from_arg_matches_mut(&mut matches)
    }
    fn update_from_arg_matches_mut(&mut self, matches: &mut ArgMatches) -> Result<(), clap::Error> {
        self.result_db = matches.get_one::<PathBuf>("result-db").unwrap().to_owned();
        Ok(())
    }
}

// This is where we actually define the arguments with dynamic defaults.
impl clap::Args for RuntimeDefaultArgs {
    fn augment_args(cmd: clap::Command) -> clap::Command {
        let default = Box::leak(Box::new(
            directories::ProjectDirs::from("", "", "local-ci")
                .expect("couldn't find user data dir")
                .data_local_dir()
                .to_owned(),
        ));

        cmd.arg(
            Arg::new("result-db")
                .long("result-db")
                .value_parser(value_parser!(PathBuf))
                .default_value(default.to_str()),
        )
    }
    fn augment_args_for_update(cmd: clap::Command) -> clap::Command {
        Self::augment_args(cmd)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Args::parse();

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

    info!("serving from {:?}", args.runtime_default.result_db.clone());
    tokio::spawn(http::serve_dir(args.runtime_default.result_db.clone()));

    let repo = git::PersistentWorktree {
        path: args.repo.to_owned().into(),
    };
    // Check repo is valid.
    repo.git_dir()
        .await
        .context(format!("opening repo {}", args.repo))?;
    let repo = Arc::new(repo);
    let manager_builder =
        config::manager_builder(repo.clone(), &args.runtime_default.result_db, &args.config)?
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
                test_manager.set_revisions(revs.clone()).await.context("setting revisions to test")?;
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
                test_manager.set_revisions([]).await.context("cancelling tests")?;
                break;
            }
        )
    }
    test_manager.settled().await;
    Ok(())
}
