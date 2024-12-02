use anyhow::{anyhow, bail, Context};
use clap::{arg, Parser as _, Subcommand, ValueEnum};
use config::{Config, ParsedConfig};
use dag::{Dag, GraphNode as _};
use database::{Database, DatabaseEntry, DatabaseOutput};
use futures::future::join_all;
use futures::StreamExt;
use git::{Commit, PersistentWorktree, TempWorktree};
use http::Ui;
use log::{debug, info};
use nix::sys::utsname::uname;
use resource::Pools;
use resource::{Resource, ResourceKey};
use std::borrow::Borrow as _;
use std::cmp::min;
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Display;
use std::io::{stdout, Stdout};
use std::path::{absolute, Path, PathBuf};
use std::pin::pin;
use std::process::Stdio;
use std::sync::Arc;
use std::{env, fmt, fs, str};
use tempfile::TempDir;
use test::{
    base_job_env, Manager, TestCase, TestCaseId, TestJob, TestJobBuilder, TestJobOutput, TestName,
};
use test::{Test, TestResult};
use tokio::select;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;
use util::{DisplayablePathBuf, ErrGroup};

use crate::git::Worktree;
use crate::terminal::TerminalSizeWatcher;

mod config;
mod dag;
mod database;
mod git;
mod http;
mod process;
mod resource;
mod terminal;
mod test;
mod text;
mod ui;
mod util;

#[cfg(test)]
mod test_utils;

#[derive(clap::Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    // TODO: Don't require valid utf-8 strings here, OsStrings shoud be fine. But
    // https://stackoverflow.com/questions/76341332/clap-default-value-for-pathbuf
    #[arg(short, long, default_value_t = {".".to_string()})]
    repo: String,
    /// Path to TOML config file. Default is $LIMMAT_CONFIG if non-empty,
    /// or ./limmat.toml if it exists, or ./.limmat.toml if it exists
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Directory where results will be stored.
    #[arg(long, default_value_t = default_result_db())]
    result_db: DisplayablePathBuf,
    /// Filename prefix for temporary worktrees.
    #[arg(long, default_value_t = {"limmat-worktree".to_string()})]
    worktree_prefix: String,
    /// Directory (must exist) to create temporary worktrees in.
    #[arg(long, default_value_t = {env::temp_dir().to_string_lossy().into_owned()})]
    worktree_dir: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args, Debug)]
struct WatchArgs {
    /// Socket address in the form "$ip:$port" to listen on for serving files
    /// over HTTP. For example "127.0.0.1:8080" or ""[::1]:1234". Set the port
    /// to 0 to let the OS pick a port for us.
    #[arg(long, default_value_t = {"0.0.0.0:0".to_string()})]
    http_sockaddr: String,
    /// Hostname to use for HTTP URLs
    #[arg(long, default_value_t = default_hostname())]
    hostname: String,
    /// Base of range to test. Will test commits between this (exclusive) and
    /// HEAD (inclusive). Whenever HEAD changes, this string will be re-evaluated
    /// to find the base of the range.
    base: String,
}

fn default_result_db() -> DisplayablePathBuf {
    DisplayablePathBuf(
        directories::ProjectDirs::from("", "", "limmat")
            .expect("couldn't find user data dir")
            .data_local_dir()
            .to_owned(),
    )
}

fn default_hostname() -> String {
    uname()
        .expect("couldn't get nodename")
        .nodename()
        .to_owned()
        .into_string()
        .expect("hostname wasn't utf-8")
}

// Returns the path we should look for the the config. Although this is
// influenced by the existence of files, it doesn't guarantee that the returned
// file exists.
fn find_config(config_arg: &Option<PathBuf>) -> anyhow::Result<PathBuf> {
    if let Some(path) = config_arg {
        return Ok(path.clone()); // (Might not exist)
    }
    if let Some(path) = env::var_os("LIMMAT_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let limmat_dot_toml = PathBuf::from("./limmat.toml");
    if limmat_dot_toml.exists() {
        return Ok(limmat_dot_toml);
    }
    let dot_limmat_dot_toml = PathBuf::from("./.limmat.toml");
    if dot_limmat_dot_toml.exists() {
        return Ok(dot_limmat_dot_toml);
    }
    bail!("Neither config nor $LIMMAT_CONFIG were set. No ./limmat.toml or ./.limmat.toml found");
}

#[derive(clap::Args, Debug)]
struct TestArgs {
    /// Name of the test to run, per the "name" field in the config file.
    test: String,
}

#[derive(clap::Args, Debug)]
struct GetArgs {
    /// Name of the test to run, per the "name" field in the config file.
    test: String,
    /// Whether to run the test if the result is not in the database.
    #[arg(long, default_value_t = false)]
    run: bool,
    /// Revision to test. Any git revspec is fine.
    rev: String,
    /// Which output from the job do we want?
    #[arg(default_value_t = GetOutput::Stdout)]
    output: GetOutput,
}

#[derive(Clone, ValueEnum, Debug)]
enum GetOutput {
    Stdout,
    Stderr,
    Artifacts,
}

impl Display for GetOutput {
    fn fmt(&self, w: &mut fmt::Formatter) -> fmt::Result {
        write!(
            w,
            "{}",
            match self {
                Self::Stdout => "stdout",
                Self::Stderr => "stderr",
                Self::Artifacts => "artifacts",
            }
        )
    }
}

#[derive(Subcommand, Debug)]
enum Command {
    /// The main command. Watch a repository and run tests whenever the revision
    /// range changes.
    Watch(WatchArgs),
    /// Run a one-shot test in the specified repo. Do not cache the results.
    Test(TestArgs),
    /// EXPERIMENTAL: Get the path of a test's output in the result database.
    Get(GetArgs),
}

// Kitchen-sink object for global shit.
struct Env {
    config: ParsedConfig,
    repo: Arc<git::PersistentWorktree>,
    database: Arc<Database>,
    worktree_builder: WorktreeBuilder,
}

// Fallback instead of https://github.com/Stebalien/tempfile/pull/308
struct WorktreeBuilder {
    prefix: OsString,
    parent_dir: PathBuf,
}

impl WorktreeBuilder {
    pub fn build(&self) -> anyhow::Result<TempDir> {
        tempfile::Builder::new()
            .prefix(&self.prefix)
            .tempdir_in(&self.parent_dir)
            .context("creating temp dir for worktree")
    }
}

// This is the main loop of the program. Take notifications from the Git tree,
// feed them to the test manager, feed the test manager's results to the status
// tracker (basically the UI).
async fn watch_loop(
    cancellation_token: CancellationToken,
    test_manager: Arc<test::Manager<PersistentWorktree>>,
    mut status_tracker: ui::StatusTracker<PersistentWorktree, Stdout>,
    range_spec: OsString,
    repo: Arc<PersistentWorktree>,
) -> anyhow::Result<()> {
    let mut revs_stream = pin!(repo.watch_refs(&range_spec)?);
    let mut notifs = test_manager.results();

    let size_watcher = TerminalSizeWatcher::new()?;
    let mut resizes = pin!(size_watcher.resizes());

    loop {
        select! {
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
                status_tracker.repaint(&size_watcher.size()).context("error painting status to stdout")?;
            },
            notif = notifs.recv() => {
                // https://github.com/rust-lang/futures-rs/issues/1857
                // AFAICS there is no way to encode a stream that never terminates.
                let notif = notif.expect("notification stream terminated");
                status_tracker.update(notif);
                status_tracker.repaint(&size_watcher.size()).context("error painting status to stdout")?;
            },
            _ = resizes.next() => {
                status_tracker.repaint(&size_watcher.size()).context("error painting status to stdout")?;
            },
            _ =  cancellation_token.cancelled() => {
                info!("Got shutdown signal, terminating jobs and waiting");
                test_manager.cancel_running().await.context("cancelling tests")?;
                // Ensure jobs are shut down before we delort stuff etc.
                test_manager.settled().await;
                break;
            }
        }
    }
    Ok(())
}

async fn watch(
    env: Env,
    cancellation_token: CancellationToken,
    watch_args: WatchArgs,
) -> anyhow::Result<()> {
    let mut eg = ErrGroup::new(cancellation_token.clone());

    // Create HTTP server, to serve the result artifacts to the user when they
    // click terminal hyperlinks.
    let listener = tokio::net::TcpListener::bind(watch_args.http_sockaddr.clone())
        .await
        .context("setting up HTTP server")?;
    let ui = Ui::new(
        watch_args.hostname.clone(),
        listener,
        env.database.base_dir.clone(),
        format!(
            "Limmat | {}",
            absolute(env.repo.path())
                .context("error getting absolute path of repo")?
                .file_name()
                .map(|n| n.to_string_lossy())
                .unwrap_or("<unknown>".into())
        ),
    );
    let result_url_base = ui.result_url_base()?;
    let home_url = ui.home_url()?;
    let ui_state = ui.state();
    eg.spawn(ui.serve(cancellation_token.child_token()));

    // Set up the test manager, which is the weirdly-scoped god-object that
    // orchestrates test jobs.
    let test_manager = Arc::new(Manager::new(
        env.repo.clone(),
        env.database,
        env.config.resource_pools.clone(),
        env.config.tests,
    ));

    // Set up the status tracker, which shows the user what's going on in the terminal.
    let status_tracker = ui::StatusTracker::new(
        env.repo.clone(),
        stdout(),
        ui_state,
        result_url_base,
        home_url,
    );

    // Kick off creation of the worktrees that the test manager will run jobs in.
    //
    // Once we've done this, we can no longer return from this function until
    // we've also cleaned the worktrees up. This is stinky and gross. AFAICT
    // async Rust just doesn't have a solution for that at all.
    //
    // TODO: This doesn't work if there are no commits in the repository. Not sure I care about
    // this, but the solution would be to create the worktrees ondemand, when we have a revision we
    // are actually trying to test. That might be a good idea anyway, so probably it's preferable to
    // just do that for its own sake and leave the empty-repo problem as a nice freebie.
    for _ in 0..env.config.num_worktrees {
        let repo = env.repo.clone();
        let ct = cancellation_token.child_token();
        let resource_pools = env.config.resource_pools.clone();
        let dir = env.worktree_builder.build()?;
        eg.spawn(async move {
            let worktree = TempWorktree::new::<PersistentWorktree>(&ct, repo.as_ref(), dir).await?;
            resource_pools.add([(ResourceKey::Worktree, Resource::Worktree(worktree))]);
            Ok(())
        });
    }

    // DO THE THING.
    eg.spawn(watch_loop(
        cancellation_token.child_token(),
        test_manager.clone(),
        status_tracker,
        format!("{}..HEAD", watch_args.base).into(),
        env.repo,
    ));

    let end_result = eg.wait().await;

    // Now we have to remember to clean up before returning the result :/
    join_all(
        Arc::into_inner(test_manager)
            .expect("leaked test manager reference")
            .into_resource_pools()
            .try_remove_worktrees()
            .map(|w| w.cleanup()),
    )
    .await;

    end_result
}

async fn ensure_job_success(
    database: Arc<Database>,
    resource_pools: Arc<Pools>,
    mut job: TestJob<DatabaseOutput>,
    origin_worktree: PathBuf,
) -> anyhow::Result<()> {
    job.await_dep_success()
        .await
        .map_err(|name| anyhow!("dependency job {name:?} failed"))?;
    job.run(database, resource_pools.as_ref(), &origin_worktree)
        .await
        .into()
}

struct OneshotOutput {
    artifacts_dir: PathBuf,
}

impl TestJobOutput for OneshotOutput {
    fn stdout(&mut self) -> anyhow::Result<Stdio> {
        Ok(Stdio::inherit())
    }
    fn stderr(&mut self) -> anyhow::Result<Stdio> {
        Ok(Stdio::inherit())
    }
    fn set_result(&mut self, result: &TestResult) -> anyhow::Result<()> {
        eprintln!("Job result: {result:?}");
        Ok(())
    }
    fn artifacts_dir(&mut self) -> anyhow::Result<&Path> {
        Ok(&self.artifacts_dir)
    }
}

// Run a set of tests at a given version, in worktrees, in parallel, unless
// there's already a result in the database. Error if any fail.
async fn ensure_tests_run(
    env: &Env,
    cancellation_token: CancellationToken,
    tests: Vec<&Arc<Test>>,
    rev: &Commit,
) -> anyhow::Result<()> {
    let tests = tests.into_iter();
    let num_worktrees = min(
        env.config.num_worktrees,
        tests.clone().filter(|t| t.needs_worktree()).count(),
    );

    let job_env = Arc::new(base_job_env(env.repo.path()));

    // Get the graph of tests we need to run as dependencies.
    // This is kinda inefficient: we're building a new Dag based on a subset of
    // the old one, so the validation in the constructor is not strictly
    // necessary. There are two levels of optimisation we could do here:
    // 1. We could add a way to build the new Dag from the TopDown iterator
    //    or some derivative of it, and use the type safety to just skip
    //    validation
    // 2. We could build the subset graph in place, i.e. totally skip making a
    //    new graph and instead just logicall remove the nodes we don't need.
    let jobs = Dag::new(
        // TODO: Would be nice to have an _into thing so we can avoid this clone.
        tests.map(|t| TestCase::new(rev.clone(), t.clone())),
    )
    .context("setting up dependency test graph")?
    .bottom_up()
    .try_fold(
        HashMap::new(),
        |mut jobs, test_case| -> anyhow::Result<HashMap<TestCaseId, TestJob<DatabaseOutput>>> {
            let wait_for = test_case
                .child_ids() // This gives the TestCaseIds of dependency jobs.
                .iter()
                .map(|tc_id| {
                    (
                        test_case.test.name.clone(),
                        jobs[tc_id.borrow()].subscribe_completion(),
                    )
                })
                .collect();
            let job = TestJobBuilder::new(
                cancellation_token.clone(),
                // TODO: it would be nice if we had an into_ variant of
                // the bottom_up so we didn't need this clone.
                test_case.clone(),
                env.database.create_output(test_case)?,
                job_env.clone(),
                wait_for,
            )
            .build();
            jobs.insert(test_case.id().borrow().to_owned(), job);
            Ok(jobs)
        },
    )?;

    // Kick off creation of the worktrees that the dep jobs will run in.
    // This is horribly copy-pasted from watch. I dunno, I can't figure out how
    // to fix that without insanely complex async stuff.
    let mut eg = ErrGroup::new(cancellation_token.clone());
    for _ in 0..num_worktrees {
        let repo = env.repo.clone();
        let ct = cancellation_token.child_token();
        let resource_pools = env.config.resource_pools.clone();
        let dir = env.worktree_builder.build()?;
        eg.spawn(async move {
            let worktree = TempWorktree::new::<PersistentWorktree>(&ct, repo.as_ref(), dir).await?;
            resource_pools.add([(ResourceKey::Worktree, Resource::Worktree(worktree))]);
            Ok(())
        });
    }

    for (_, job) in jobs {
        eg.spawn(ensure_job_success(
            env.database.clone(),
            env.config.resource_pools.clone(),
            job,
            env.repo.path().to_owned(),
        ));
    }

    let end_result = eg.wait().await;

    // Now we have to remember to clean up before returning the result :/
    join_all(
        env.config
            .resource_pools
            .try_remove_worktrees()
            .map(|w| w.cleanup()),
    )
    .await;

    end_result
}

async fn test(
    env: Env,
    cancellation_token: CancellationToken,
    test_args: &TestArgs,
) -> anyhow::Result<()> {
    let test_name = TestName::new(test_args.test.clone());
    // So we can cache the results in the database, the dependency jobs will be run at HEAD.
    let head = env
        .repo
        .rev_parse("HEAD")
        .await
        .context("failed to look up HEAD commit")?
        .ok_or(anyhow!("no HEAD commit - repo empty?"))?;

    // Only need worktrees for the tests that needs worktrees.
    let dep_tests: Vec<&Arc<Test>> = env
        .config
        .tests
        .top_down_from(&test_name)
        .ok_or(anyhow!("no such test {:?}", test_name.to_string()))?
        // Exclude the main test, we'll run that separately.
        .skip(1)
        .collect();

    if !dep_tests.is_empty() {
        eprintln!("Running {} dependency jobs...", dep_tests.len());
        ensure_tests_run(&env, cancellation_token.child_token(), dep_tests, &head).await?;
        eprintln!("Dependency jobs complete.");
    }

    let artifacts_dir = TempDir::with_prefix("limmat-artifacts-")?.into_path();
    eprintln!("Setting $LIMMAT_ARTIFACTS to {}", artifacts_dir.display());

    let test = env.config.tests.node(&test_name).unwrap();
    let test_case = TestCase::new(head.clone(), test.clone());
    let mut needs_resources = test_case.test.needs_resources.clone();

    let job = TestJobBuilder::new(
        cancellation_token.clone(),
        test_case,
        OneshotOutput { artifacts_dir },
        Arc::new(base_job_env(env.repo.path())),
        Vec::new(), // wait_for
    )
    .build();
    // Doesn't need a worktree, it's gonna do it live and direct in the main tree.
    needs_resources.remove(&ResourceKey::Worktree);
    let resources = env.config.resource_pools.get(needs_resources).await;
    let status = job.run_with_resources(env.repo.path(), &resources).await;
    eprintln!("Finished: {}", status);
    status.into()
}

// Run a single test at the revision specified in the args.
async fn get_common(
    env: Env,
    cancellation_token: CancellationToken,
    get_args: &GetArgs,
) -> anyhow::Result<DatabaseEntry> {
    let test_name = TestName::new(get_args.test.clone());
    let rev = env
        .repo
        .rev_parse(&get_args.rev)
        .await
        .context("error looking up commit")?
        .ok_or_else(|| {
            anyhow!(
                "revision {:?} not found in {}",
                get_args.test,
                env.repo.path().display()
            )
        })?;

    if get_args.run {
        let tests: Vec<&Arc<Test>> = env
            .config
            .tests
            .top_down_from(&test_name)
            .ok_or(anyhow!("no such test {:?}", test_name.to_string()))?
            .collect();

        // Write to stderr so the output can just be the path, for scripting.
        eprintln!("Running {} tests...", tests.len());
        ensure_tests_run(&env, cancellation_token.child_token(), tests, &rev).await?;
        eprintln!("Tests complete");
    }

    let test = env
        .config
        .tests
        .node(&test_name)
        .ok_or(anyhow!("no such test {:?}", test_name.to_string()))?;
    env.database
        .lookup_result(&TestCase::new(rev.clone(), test.clone()))
        .context("looking up result in database")?
        .ok_or_else(|| {
            anyhow!(
                "no database entry for test {:?} at revision {:?} ({})",
                test_name.to_string(),
                get_args.rev,
                rev.hash
            )
        })
}

async fn get(
    env: Env,
    cancellation_token: CancellationToken,
    get_args: GetArgs,
) -> anyhow::Result<()> {
    let db_entry = get_common(env, cancellation_token, &get_args).await?;
    match get_args.output {
        GetOutput::Stdout => println!("{}", db_entry.stdout_path().display()),
        GetOutput::Stderr => println!("{}", db_entry.stderr_path().display()),
        GetOutput::Artifacts => println!("{}", db_entry.artifacts_dir().display()),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Set up shutdown first, to ensure we correctly handle early signals.
    // It seems extremely difficult to use Tokio's ctrl_c API for this correctly
    // (It only installs the handler the first time the signal gets polled)
    // so we just go directly for the "interrupt" thing, which might not work on
    // Windows, not sure. (I don't think this code is likely to work on Windows
    // anyway).
    let cancellation_token = CancellationToken::new();
    let mut sigint = signal(SignalKind::interrupt()).context("registering SIGINT handler")?;
    let token = cancellation_token.clone();
    tokio::spawn(async move {
        sigint.recv().await;
        token.cancel()
    });

    let args = Args::parse();
    debug!("args: {:?}", &args);
    let config_content =
        fs::read_to_string(&find_config(&args.config)?).context("couldn't read config")?;
    debug!("config:\n{}", &config_content);
    let config: Config = toml::from_str(&config_content).context("couldn't parse config")?;
    let config = ParsedConfig::from(config)?;

    let repo = git::PersistentWorktree {
        path: args.repo.to_owned().into(),
    };
    // Check repo is valid.
    repo.git_common_dir()
        .await
        .context(format!("opening repo {}", args.repo))?;

    let env = Env {
        config,
        repo: Arc::new(repo),
        database: Arc::new(Database::create_or_open(&args.result_db)?),
        worktree_builder: WorktreeBuilder {
            prefix: args.worktree_prefix.into(),
            parent_dir: args.worktree_dir.into(),
        },
    };

    match args.command {
        Command::Watch(watch_args) => watch(env, cancellation_token, watch_args).await,
        Command::Test(ref test_args) => test(env, cancellation_token, test_args).await,
        Command::Get(get_args) => get(env, cancellation_token, get_args).await,
    }
}
