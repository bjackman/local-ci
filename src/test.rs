use core::fmt;
use core::fmt::Display;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::ffi::OsString;
use std::fmt::Debug;
use std::fmt::Formatter;
use std::path::Path;
use std::path::PathBuf;
use std::pin::pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::bail;
use anyhow::{anyhow, Context};
use futures::future::select_all;
use futures::future::FutureExt;
use futures::future::{self, try_join_all, Either};
use itertools::Itertools;
use log::debug;
use log::error;
use log::info;
use log::warn;
use nix::sys::signal::kill;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use serde::Deserialize;
use serde::Serialize;
use tokio::process::Command;
use tokio::select;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::git::TempWorktree;
use crate::git::{CommitHash, Hash, Worktree};
use crate::process::OutputExt;
use crate::resource::Pools;
use crate::resource::Resource;
use crate::resource::ResourceKey;
use crate::resource::Resources;
use crate::result::Database;
use crate::result::TestCaseOutput;

pub trait ResultExt {
    // Log an error if it occurs, prefixed with s, otherwise return nothing.
    fn or_log_error(&self, s: &str);
}

impl<T, E> ResultExt for Result<T, E>
where
    E: Display,
{
    fn or_log_error(&self, s: &str) {
        if let Err(e) = self {
            error!("{} - {}", s, e);
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    NoCaching,
    ByCommit,
    ByTree,
}

// Some unspecified hash, don't care too much about stability across builds.
pub type ConfigHash = u64;

#[derive(Clone, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct TestName(String);

impl TestName {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
}

impl AsRef<Path> for TestName {
    fn as_ref(&self) -> &Path {
        Path::new(OsStr::new(&self.0))
    }
}

impl Display for TestName {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// A test task that will need to be repeated for each commit.
// TODO: this struct is too complex for the plain-old-data (pub fields)
// approach, it should be constructed with a builder.
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct Test {
    pub name: TestName,
    // Hash of the configuration that created this Test.
    pub config_hash: ConfigHash,
    pub program: OsString,
    pub args: Vec<OsString>,
    // Counts of the resource tokens this test needs a resource-token before it
    // can begin.
    pub needs_resources: HashMap<ResourceKey, usize>,
    pub shutdown_grace_period: Duration,
    pub cache_policy: CachePolicy,
    // This tests shoudln't start until these other tests have finished.
    // Manager setup will fail if there are cycles in this graph or named tests
    // do not exist.
    pub depends_on: Vec<TestName>,
}

impl Test {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        // Separate process group means the child doesn't get SIGINT if the user
        // Ctrl-C's the terminal.
        cmd.process_group(0);
        // Ensure we don't pass random nonsense to the test command and create
        // confusing behaviour. This is kinda annoying because IIUC this gives
        // you a fd that is immediately closed, which is likely to be different
        // from the environment where you're testing your scripts (i.e. a shell
        // prompt). You'd think just using Stdio::piped() would give a stdin
        // that is open but has nothing on it, but that isn't th behavoiur I've
        // observed, I'm not too sure why but don't wanna keep debugging this
        // forever.
        cmd.stdin(Stdio::null());
        cmd
    }
}

impl Display for Test {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<test: {:?}>", self.name)
    }
}

impl Debug for Test {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self)
    }
}

pub struct ManagerBuilder<W> {
    // This needs to be an Arc because we hold onto a reference to it for a
    // while, and create temporary worktrees from it in the background.
    repo: Arc<W>,
    tests: Vec<Test>,
    resource_tokens: HashMap<ResourceKey, Vec<String>>,
    result_db: Database,

    num_worktrees: usize,
    worktree_prefix: String,
    worktree_dir: PathBuf,
    job_env: Vec<(String, String)>,
}

impl<W> ManagerBuilder<W> {
    pub fn num_worktrees(mut self, n: usize) -> Self {
        self.num_worktrees = n;
        self
    }

    // Worktree temp-directories will have their name (not path!) prefixed with this.
    pub fn worktree_prefix(mut self, prefix: &str) -> Self {
        prefix.clone_into(&mut self.worktree_prefix);
        self
    }

    // Directory to create worktrees in
    pub fn worktree_dir<P: Into<PathBuf>>(mut self, dir: P) -> Self {
        self.worktree_dir = dir.into();
        self
    }

    fn check_no_cycles(tests: &Vec<Test>) -> anyhow::Result<()> {
        let name_to_idx: HashMap<TestName, usize> = tests
            .iter()
            .enumerate()
            .map(|(i, t)| (t.name.clone(), i))
            .collect();
        // We'll assume the graph is pretty small and not try to do anything
        // clever here. Just do a DFS from each starting point and maintain a
        // set of observed nodes, using the simplest possible code even if it
        // means pointless copies.
        // let check = |start_idx: usize, seen: HashSet<String>| -> anyhow::Result<()> {
        //
        // This is a bit annoying in Rust because you cannot capture
        // environments into a named function but you cannot recurse into a
        // closure, so we just have to pass everything though args explicitly.
        fn check(
            tests: &Vec<Test>,
            start_idx: usize,
            seen: &HashSet<TestName>,
            name_to_idx: &HashMap<TestName, usize>,
        ) -> anyhow::Result<()> {
            let start_test = &tests[start_idx];
            if seen.contains(&start_test.name) {
                bail!("Cycle in test dependency graph");
            }
            let mut seen = seen.clone();
            seen.insert(start_test.name.clone());
            for dep_name in start_test.depends_on.iter() {
                check(tests, name_to_idx[dep_name], &seen, name_to_idx)?;
            }
            Ok(())
        }
        for i in 0..tests.len() {
            check(tests, i, &HashSet::new(), &name_to_idx)?;
        }
        Ok(())
    }

    // Starts the workers. You must call close() before dropping it.
    //
    // TODO: This doesn't work if there are no commits in the repository. Not sure I care about
    // this, but the solution would be to create the worktrees ondemand, when we have a revision we
    // are actually trying to test. That might be a good idea anyway, so probably it's preferable to
    // just do that for its own sake and leave the empty-repo problem as a nice freebie.
    pub async fn build(self) -> anyhow::Result<Manager<W>>
    where
        // We need to specify 'static here. Just because we have an Arc over the
        // repo that doesn't mean it automatically satisfies 'static:
        // https://users.rust-lang.org/t/why-is-t-static-constrained-when-using-arc-t-and-thread-spawn/26262/2
        // It would be much more convenient to just specify some or all these
        // trait bounds as subtraits of Workrtree. But I dunno, that feels Wrong.
        W: Worktree + Sync + Send + 'static,
    {
        Self::check_no_cycles(&self.tests)?;
        info!(
            "Setting up {} worktrees in {:?}...",
            self.num_worktrees, self.worktree_dir
        );
        let worktrees = try_join_all((0..self.num_worktrees).map(|_| async {
            // Not doing this async because I assume it's fast, there is no white-glove support,
            // and the drop will have to be synchronous anyway.
            let t = tempfile::Builder::new()
                .prefix(&self.worktree_prefix)
                .tempdir_in(&self.worktree_dir)
                .context("creating temp dir for worktree")?;
            TempWorktree::new::<W>(self.repo.borrow(), t).await
        }))
        .await
        .context("setting up temporary worktrees")?;
        info!("Worktree setup done.");

        let Self {
            repo,
            tests,
            result_db,
            resource_tokens,
            job_env,
            num_worktrees: _,
            worktree_prefix: _,
            worktree_dir: _,
        } = self;

        // Combine the worktrees and generic tokens into reosurces that can be
        // managed by the resource module.
        let mut resources: HashMap<ResourceKey, Vec<Resource>> = resource_tokens
            .into_iter()
            .map(|(key, tokens)| (key, tokens.into_iter().map(Resource::UserToken).collect()))
            .collect();
        resources.insert(
            ResourceKey::Worktree,
            worktrees.into_iter().map(Resource::Worktree).collect(),
        );

        // TODO: If this capacity gets exhausted, data gets lost and we get an error which this code
        // probably doesn't handle very gracefully. We should instead just block the sender.
        let (result_tx, _) = broadcast::channel(4096);
        Ok(Manager {
            job_env: Arc::new(job_env),
            repo,
            result_tx,
            job_cts: HashMap::new(),
            job_counter: JobCounter::new(),
            tests: tests.into_iter().map(Arc::new).collect(),
            resource_pools: Arc::new(Pools::new(resources)),
            result_db,
        })
    }
}

// Manages a bunch of worker threads that run tests for the current set of revisions.
pub struct Manager<W: Worktree> {
    repo: Arc<W>,
    job_cts: HashMap<TestCaseId, CancellationToken>,
    job_counter: JobCounter,
    result_tx: broadcast::Sender<Arc<Notification>>,
    tests: Vec<Arc<Test>>,
    // Pools contains sets of intangible arbitrary "resources" that can be used to throttle test
    // jobs, and also tracks access to reused worktrees. The indices of the token-type resources
    // will be referenced by Test::needs_resource_idx values.
    resource_pools: Arc<Pools>,
    result_db: Database,
    job_env: Arc<Vec<(String, String)>>,
}

impl<W: Worktree + Sync + Send + 'static> Manager<W> {
    pub fn builder<T, R>(
        // This needs to be an Arc because we hold onto a reference to it for a
        // while, and create temporary worktrees from it in the background.
        repo: Arc<W>,
        // This is mandatory instead of defaulting to the user's main database,
        // because we want it to be hard to accidentally refer to global
        // resources like that.
        result_db: Database,
        tests: T,
        // Tokens for resources, basically a HashMap from "resource type" names
        // to token values.
        resource_tokens: R,
    ) -> ManagerBuilder<W>
    where
        T: IntoIterator<Item = Test>,
        R: IntoIterator<Item = (ResourceKey, Vec<String>)>,
    {
        ManagerBuilder {
            tests: tests.into_iter().collect(),
            resource_tokens: resource_tokens.into_iter().collect(),
            result_db,

            num_worktrees: 1,
            worktree_prefix: "worktree-".to_owned(),
            worktree_dir: env::temp_dir(),
            job_env: vec![(
                "LCI_ORIGIN".into(),
                repo.path().to_string_lossy().into_owned(),
            )],

            repo,
        }
    }

    async fn cache_lookup(&self, test_case: &TestCase) -> Option<TestResult> {
        match test_case.cache_hash {
            Some(ref hash) => {
                match self
                    .result_db
                    .cached_result(hash, &test_case.test.name, test_case.test.config_hash)
                    .context("reading cached test result")
                {
                    Err(err) => {
                        error!("Failed to read cached test result, will overwrite: {err:?}");
                        None
                    }
                    Ok(maybe_result) => maybe_result,
                }
            }
            None => None,
        }
    }

    fn notify(&self, test_case: TestCase, status: TestStatus) {
        self.result_tx
            .send(Arc::new(Notification { test_case, status }))
            .or_log_error("Dropping a notification. Probably nothing was listening");
    }

    async fn spawn_job(&self, mut job: TestJob) {
        if let Some(test_result) = self.cache_lookup(&job.test_case).await {
            let result = TestStatus::Completed(test_result);
            job.notifier.send(result.clone());
            self.notify(job.test_case, result);
            return;
        }

        self.notify(job.test_case.clone(), TestStatus::Enqueued);

        let tx = self.result_tx.clone();
        let pools = self.resource_pools.clone();
        let origin_worktree = self.repo.clone();
        tokio::spawn(async move {
            // Wait for dependencies do be done, bail early if they do anything
            // but terminate successfully.
            if let Err(failed_test_name) = job.await_dep_success().await {
                info!(
                    "{:?} canceled due to unsuccess of dependency {failed_test_name:?}",
                    job.test_case
                );
                // TODO: avoid all this duplicative notifying
                let status =
                    TestStatus::Error(format!("Dependency {failed_test_name:?} unsuccessful"));
                tx.send(Arc::new(Notification {
                    test_case: job.test_case.clone(),
                    status: status.clone(),
                }))
                .or_log_error("Dropping a notification");
                job.notifier.send(status);
                return;
            }

            // This "biased" is here because otherwise when we cancel a bunch of jobs all at once,
            // and some of those jobs are blocking on resources held by others,
            // we want the former jobs to observe their own cancellation before
            // they see the resources get freed up by the latter. I don't think
            // this totally eliminates that case, which probably means tests
            // will be flaky. Not sure what to do about that.
            select!(biased;
                    _ = job.ct.cancelled() => (),
                    resources = pools.get(job.test_case.test.needs_resources.clone()) =>  {
                tx.send(Arc::new(Notification {
                    test_case: job.test_case.clone(),
                    status: TestStatus::Started,
                }))
                .or_log_error("Dropping a notification");
                let result = if let Some(worktrees) = resources.resources(&ResourceKey::Worktree) {
                    // We "own" this worktree.
                    job.checkout_and_run(worktrees[0].as_worktree(), &resources).await
                } else {
                    // We don't "own" the "main" worktree so the job shouldn't mess with it.
                    job.run(origin_worktree.path(), &resources).await
                };
                let status = match result {
                    Err(ref err) => TestStatus::Error(err.to_string()),
                    Ok(None) => TestStatus::Canceled,
                    Ok(Some(exit_code)) => {
                        let test_result = TestResult{exit_code};
                        job.output
                            .set_result(&test_result)
                            .or_log_error("couldn't save job status");
                        TestStatus::Completed(test_result)
                    }
                };
                // Note: must not drop test until the send is complete, or we would break
                // settled().
                let _ = tx.send(Arc::new(Notification {
                    test_case: job.test_case,
                    status,
                }))
                .map_err(|e|
                    error!("Dropping a result ({result:?}. Seems nobody is listening to Manager::results(): {}", e)
                );
            });
        });
    }

    // Interrupt any revisions that are not in revs, start testing all revisions in revs that are
    // not already tested or being tested.
    // It doesn't make sense to call this function if you don't have a receiver
    // from already having called [[results]].
    pub async fn set_revisions<I>(&mut self, revs: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = CommitHash>,
    {
        // Build the set test cases we need to kick off.
        let test_cases = try_join_all(
            revs.into_iter()
                .cartesian_product(self.tests.iter())
                .map(|(rev, test)| TestCase::new(rev, test.clone(), self.repo.as_ref())),
        )
        .await
        .context("setting up test cases")?;
        let test_cases: HashMap<TestCaseId, TestCase> =
            test_cases.into_iter().map(|tc| (tc.id(), tc)).collect();

        // Cancel jobs for test cases that we don't care about any more.
        // https://github.com/rust-lang/rust/issues/59618 would make this more convenient.
        self.job_cts = self
            .job_cts
            .drain()
            .filter(|(id, cancellation_token)| {
                if !test_cases.contains_key(id) {
                    cancellation_token.cancel();
                    return false;
                }
                true
            })
            .collect();

        // Now start the new jobs.
        let mut jobs = test_cases
            .into_iter()
            // Don't start new jobs for test cases that are already running
            .filter(|(tc_id, _)| !self.job_cts.contains_key(tc_id))
            .map(
                |(tc_id, test_case)| -> anyhow::Result<(TestCaseId, TestJob)> {
                    Ok((
                        tc_id,
                        TestJob {
                            ct: CancellationToken::new(),
                            _token: self.job_counter.get(),
                            output: self.result_db.create_output(&test_case)?,
                            test_case,
                            env: self.job_env.clone(),
                            // We don't have anything to subscribe to yet, leave
                            // this empty at first and populate later.
                            wait_for: Vec::new(),
                            notifier: JobDepNotifier::new(),
                        },
                    ))
                },
            )
            .collect::<anyhow::Result<HashMap<TestCaseId, TestJob>>>()?;

        // Now we set up subscriptions to notify inter-job dependency
        // completion. Rust really hates to mutate one map value based on
        // another value in the same map, so we do this via an intermediate map
        // :(
        let mut wait_subs: HashMap<TestCaseId, Vec<(TestName, watch::Receiver<TestStatus>)>> = jobs
            .iter()
            .map(|(id, job)| {
                (
                    id.clone(),
                    job.test_case
                        .test
                        .depends_on
                        .iter()
                        .map(|dep_name| {
                            (
                                dep_name.clone(),
                                jobs[&TestCaseId::new(&job.test_case.commit_hash, dep_name)]
                                    .notifier
                                    .subscribe(),
                            )
                        })
                        .collect(),
                )
            })
            .collect();
        for (id, job) in jobs.iter_mut() {
            job.wait_for = wait_subs.remove(id).unwrap();
        }

        for (tc_id, job) in jobs.into_iter() {
            self.job_cts.insert(tc_id.clone(), job.ct.clone());
            self.spawn_job(job).await;
        }
        Ok(())
    }

    // Streams results back. Note you need to call this _before_ you generate the results you want
    // to receive.
    //
    // I think the "proper" solution for this is to return a Stream. But I don't understand it.
    pub fn results(&self) -> broadcast::Receiver<Arc<Notification>> {
        self.result_tx.subscribe()
    }

    // Completes once there are no pending jobs or results.
    pub async fn settled(&self) {
        self.job_counter.zero().await;
    }
}

struct JobDepNotifier {
    tx: watch::Sender<TestStatus>,
}

impl JobDepNotifier {
    fn new() -> Self {
        // This watch API is a bit weird; it's designed on the assumption that
        // there is some shared value and you want to be notified when that
        // value is changed. So when you construct a channel you pass in the
        // initial value, and the rx will immediately "receive" that value.
        // That isn't what we're using this for - we just wanna send a single
        // message, so we pass a random dummy as the "initial" value and drop
        // the rx. Then to create the actual receivers we call subscribe, since
        // that doesn't lead to the same weird "channel starts out full"
        // semantics.
        let (tx, _rx) = watch::channel(TestStatus::Started);
        Self { tx }
    }

    fn subscribe(&self) -> watch::Receiver<TestStatus> {
        self.tx.subscribe()
    }

    // Report the status of a test. Will be observed by anyone who has already
    // subscribed. The nature of the channel we're using here means that we can
    // only ever reliably send one message. If we got that wrong the results
    // would be confusing to debug, so that's why sending the message consumes
    // the tx side of the channel.  Returns an error if there is nobody
    // listening on the inner channel.
    fn send(self, status: TestStatus) {
        // Inner failure means nobody is listening. This is fine and normal.
        let _ = self.tx.send(status);
    }
}

// This is a horrible attempt to implement Manager::settled. There is no Condvar in tokio or
// futures-rs, so we have this weird condvar-like construction using a Tokio watch channel.
struct JobCounter {
    w: watch::Sender<usize>,
}

impl JobCounter {
    pub fn new() -> Self {
        Self {
            w: watch::Sender::new(0),
        }
    }

    // Increment the counter. It is decremented when the token is dropped.
    pub fn get(&self) -> JobToken {
        // Hack? We only report that we "modified" the value if it changed its
        // zeroness, since that's the only thing that waiters care about.
        self.w.send_if_modified(|count| {
            let was_zero = *count == 0;
            *count += 1;
            was_zero
        });
        JobToken { w: self.w.clone() }
    }

    // Block until the counter is zero. If it's already zero, return immediately. This might miss
    // transient zeroness but is guaranteed to return eventually if the counter stays zero for some
    // finite amount of time.
    pub async fn zero(&self) {
        let mut rx = {
            if *self.w.borrow() == 0 {
                return;
            }
            // Note there's a race here, we've already dropped the Ref from self.w.borrow() so the
            // counter could change. This doesn't matter because wait_for checks if the value is
            // already zero before blocking, so missed updates are harmless.
            self.w.subscribe()
        };
        rx.wait_for(|count| *count == 0)
            .await
            .expect("sender dropped in job counter");
    }
}

struct JobToken {
    w: watch::Sender<usize>,
}

impl Drop for JobToken {
    fn drop(&mut self) {
        self.w.send_if_modified(|count| {
            *count -= 1;
            *count == 0
        });
    }
}

// This is not really a proper type, it doesn't really mean anything except as an implementation
// detail of its user. I tried to get rid of it but then you run into issues with getting references
// to individual fields while a mutable reference exists to the overall struct. I think this is
// basically one an instance of "view structs" described in
// https://smallcultfollowing.com/babysteps/blog/2024/06/02/the-borrow-checker-within/
struct TestJob {
    ct: CancellationToken,
    test_case: TestCase,
    _token: JobToken,
    output: TestCaseOutput,
    env: Arc<Vec<(String, String)>>,
    // Job shouldn't start until all of these channels produce a result. If any
    // is unsuccessful it should abort.
    wait_for: Vec<(TestName, watch::Receiver<TestStatus>)>,
    // To avoid waking up every job task unnecessarily via the main notification
    // channel, we use a separate, internal, notifier mechanism for this
    // specific purpose.
    notifier: JobDepNotifier,
}

impl<'a> TestJob {
    // Blocks until all dependency jobs have succeeded, or returns an error
    // reporting the name of the job that terminated without success.
    async fn await_dep_success(&mut self) -> Result<(), TestName> {
        // TODO: this is a bit crazyeyyy, we're turning the map of
        // name->receiver into a vec of futures that return the name and the
        // eventual status.
        // This is another thing where the tokio::sync::watch API is a bit
        // weird, there's no way to wait for a message without passing a
        // predicate, so we have to pass a dummy one.
        let mut wait_for = std::mem::take(&mut self.wait_for);
        info!(
            "{:?}: Awaiting {} dependency jobs",
            self.test_case.test.name,
            wait_for.len()
        );
        let wait_for: Vec<_> = wait_for
            .iter_mut()
            .map(|(name, rx)| rx.wait_for(|_| true).map(|status| (name.clone(), status)))
            .collect();
        let mut wait_for: Vec<_> = wait_for.into_iter().map(Box::pin).collect();
        while !wait_for.is_empty() {
            let ((test_name, test_status), _idx, remaining) = select_all(wait_for).await;
            wait_for = remaining;
            // We are squashing lots of different types of failures and aborts
            // (including the "impossible" case that the sender has been dropped
            // and the rx.wait_for call failed) here, we trust that the other
            // side of the notifier has reported any issues appropriately.
            if let TestStatus::Completed(result) = &*test_status.map_err(|_| test_name.clone())? {
                if result.exit_code == 0 {
                    info!(
                        "{:?}: Dependency {:?} succeeded",
                        self.test_case.test.name, test_name
                    );
                    continue;
                }
            }
            return Err(test_name);
        }
        info!("{:?}: Dependencies succeeded", self.test_case);
        Ok(())
    }

    // Returns Ok(None) when canceled.
    async fn checkout_and_run<W>(
        &mut self,
        worktree: &W,
        resources: &Resources<'a>,
    ) -> anyhow::Result<Option<ExitCode>>
    where
        W: Worktree,
    {
        worktree.checkout(&self.test_case.commit_hash).await?;
        self.run(worktree.path(), resources).await
    }

    // Returns Ok(None) when canceled.
    async fn run(
        &mut self,
        current_dir: &Path,
        resources: &Resources<'a>,
    ) -> anyhow::Result<Option<ExitCode>> {
        info!("Starting {:?}", self.test_case);

        let mut cmd = self.test_case.test.command();
        let mut cmd = cmd
            .current_dir(current_dir)
            .stdout(self.output.stdout().context("no stdout handle available")?)
            .stderr(self.output.stderr().context("no stdout handle available")?)
            .env("LCI_COMMIT", self.test_case.commit_hash.to_string())
            // Killing on drop is not what we want. We really want this job to
            // get awaited so that the worktree can be safely reused and we can
            // be sure the test script has cleaned up after itself. But, in case
            // local-ci shuts down unexpectedly we'll try to at least limit the
            // damage.
            .kill_on_drop(true);
        for (k, v) in self.env.iter() {
            cmd = cmd.env(k, v);
        }
        // Set up env vars to communicate token values.
        for (resource_name, tokens) in resources.tokens() {
            for (i, token) in tokens.iter().enumerate() {
                debug!(
                    "{} = {}",
                    format!("LCI_RESOURCE_{}_{}", resource_name, i),
                    token
                );
                cmd = cmd.env(format!("LCI_RESOURCE_{}_{}", resource_name, i), token);
            }
        }
        let child = cmd.spawn().context("spawning test command")?;
        // lol wat?
        let pid = Pid::from_raw(
            child
                .id()
                .ok_or(anyhow!("no PID for child job"))?
                .try_into()
                .unwrap(),
        );
        // Await the child, or cancellation. Because the "right" branch still needs to do work on
        // the "left" future, tokio::select doesn't grant us any clarity or concision here so we
        // drop down to the raw function call.
        let child_fut = pin!(child.wait_with_output());
        let cancel_fut = pin!(self.ct.cancelled());
        match future::select(child_fut, cancel_fut).await {
            Either::Left((wait_result, _)) =>
            // Test completed, figure out the result. I think maybe a true Rustacean would
            // write this block as a single chain of methods? But it seems ridiculous to me.
            {
                let exit_code = wait_result
                    .map_err(anyhow::Error::from)?
                    .code_not_killed()?;
                Ok(Some(exit_code))
            }
            Either::Right((_, child_fut)) => {
                // Canceled. Shut down the process.
                kill(pid, Signal::SIGINT).context("couldn't interrupt child job")?;
                // We don't care about its result but we
                // need to wait for it to shut down so that we can safely give back the
                // worktree.
                let timeout = sleep(self.test_case.test.shutdown_grace_period);
                select!(
                    _ = child_fut => (),
                    _ = timeout => {
                        // Canceled. Shut down the process.
                        warn!("timeout for {:?}, SIGKILLing", self.test_case.test.name);
                        kill(pid, Signal::SIGKILL).context("couldn't interrupt child job")?;
                    }
                );

                Ok(None)
            }
        }
    }
}

// An identifier that uniquely identifies a TestCase among all that can exist for a given Manager.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TestCaseId(String);

impl TestCaseId {
    fn new(commit_hash: &CommitHash, test_name: &TestName) -> Self {
        Self(format!("{}:{}", commit_hash, test_name))
    }
}

#[derive(Clone)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub struct TestCase {
    // Commit that will be checked out to run the test.
    pub commit_hash: CommitHash,
    // Hash that will be used to identify the test result. Might be a tree hash,
    // otherwise it matches the commit hash.
    pub cache_hash: Option<Hash>,
    pub test: Arc<Test>,
}

impl Debug for TestCase {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "TestCase({:?}, {:?})",
            self.commit_hash.abbrev(),
            self.test.name
        )
    }
}

impl TestCase {
    pub async fn new<W: Worktree>(
        commit_hash: CommitHash,
        test: Arc<Test>,
        repo: &W,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            cache_hash: match test.cache_policy {
                CachePolicy::NoCaching => None::<Hash>,
                CachePolicy::ByCommit => Some(commit_hash.clone().into()),
                CachePolicy::ByTree => Some(
                    repo.commit_tree(commit_hash.clone())
                        .await
                        .context("looking up tree from commit")?
                        .into(),
                ),
            },
            test,
            commit_hash,
        })
    }

    // Returns the hash that should be used to store the result in the result
    // database. Note that results get stored in the database even when caching
    // is disabled, so that the user can see the output..
    pub fn storage_hash(&self) -> &Hash {
        self.cache_hash.as_ref().unwrap_or(&self.commit_hash)
    }

    fn id(&self) -> TestCaseId {
        // The hash_cache is redundant information here so we don't need to include it.
        TestCaseId::new(&self.commit_hash, &self.test.name)
    }
}

pub type ExitCode = i32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestStatus {
    Enqueued,
    Started,
    Canceled,
    // anyhow::Error doesn't implement Clone. We don't really need the overall
    // error handling fanciness since this is just part of the normal flow of
    // the program, so we just define this as a normal case among this enum.
    Error(String), // This includes the test getting terminated by a signal.
    Completed(TestResult),
}

impl Display for TestStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enqueued => write!(f, "Enqueued"),
            Self::Started => write!(f, "Started"),
            Self::Canceled => write!(f, "Cancelled"),
            Self::Error(msg) => write!(f, "Failed testing - {:?}", msg),
            Self::Completed(result) => write!(f, "Completed - {}", result),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    // Note this is called "exit_code" instead of "return_code" because it really
    // only gets set when the child process exits.
    pub exit_code: ExitCode,
}

impl Display for TestResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "exit code {}", self.exit_code)
    }
}

#[derive(Debug)]
pub struct Notification {
    pub test_case: TestCase,
    pub status: TestStatus,
}

#[cfg(test)]
mod tests {
    use std::{
        cmp::max,
        collections::VecDeque,
        fs::{self, remove_file, File},
        io::{self, BufRead as _},
        path::PathBuf,
        thread::panicking,
        time::Duration,
    };

    use anyhow::bail;
    use future::{join_all, select_all};
    use itertools::izip;
    use log::error;
    use tempfile::TempDir;
    use test_case::test_case;
    use tokio::{
        select,
        time::{sleep, sleep_until, Instant},
    };

    use crate::{
        git::{
            test_utils::{TempRepo, WorktreeExt},
            CommitHash,
        },
        test_utils::{path_exists, some_time, timeout_5s},
    };

    use super::*;

    // A script that can be used as the test command for a Manager, with utilities for testing the
    // manager. The script won't terminate until told to.
    struct TestScript {
        dir: TempDir,
        script: OsString, // Raw content.
        test_name: TestName,
    }

    impl TestScript {
        // Each time the script gets started it echoes a line to this file.
        const PID_FILENAME_PREFIX: &'static str = "pid.";
        const SIGINTED_FILENAME_PREFIX: &'static str = "siginted.";
        const LOCK_FILENAME: &'static str = "lockfile";
        const BUG_DETECTED_PATH: &'static str = "bug_detected";

        // If this appears in the commit message , the test script will block
        // until SIGINTed, otherwise it terminates immediately. When receiving
        // SIGINT the value depends on whether you include the results of
        // exit_code_tag(). If yes, it exits with that code, otherwise it is
        // terminated directly by the signal (the latter is considered an
        // "error" by local-ci).
        pub const BLOCK_COMMIT_MSG_TAG: &'static str = "block_this_test";

        // Generate a tag which, when put in the commit message of a commit, will result in the test
        // returning the given exit code.
        pub fn exit_code_tag(code: u32) -> OsString {
            format!("exit_code({})", code).into()
        }

        // Creates a script, this will create a temporary directory, which will
        // be destroyed on drop.
        pub fn new(test_name: TestName, use_lockfile: bool) -> Self {
            let dir = TempDir::with_prefix("test-script-").expect("couldn't make tempdir");
            // The script will touch a special file to notify us that it has been started. On
            // receiving SIGINT it touches a nother special file. Then if Terminate::Never it blocks
            // on input, which it will never receive.
            //
            // The "lockfile" lets us detect if the worktree gets assigned to multiple script
            // instances at once. We would ideally actually do this with flock but it turns out to
            // be a bit of a pain to use, so we just use regular if-statements. I _guess_ we can
            // trust from the PoV of a single thread that this will be consistent, i.e. it cannot
            // produce false positive failures. I am sure that it can produce false negatives, but
            // we could get false negatives here even with flock, since there is always a window
            // between the script starting and it actually taking the lock.
            //
            // Note that the blocking thing (maybe_read) must be a shell builtin; otherwise we would
            // need more Bash hackery to ensure that the signal gets forwarded to it.
            let script = format!(
                "trap \"touch {siginted_path_prefix:?}$(git rev-parse $LCI_COMMIT); exit\" SIGINT
                echo $$ >> {pid_path_prefix:?}$(git rev-parse $LCI_COMMIT)

                if [ -n \"{lock_filename}\" ]; then 
                    if [ -e ./{lock_filename:?} ]; then
                        echo 'Overlapping test script runs used the same worktree' >> {bug_detected_path:?}
                    fi
                    trap \"rm {lock_filename:?}\" EXIT
                    touch ./{lock_filename:?}
                fi
                commit_msg=\"$(git log -n1 --format=%B $LCI_COMMIT)\"
                exit_code=$(echo \"$commit_msg\" | perl -n -e'/exit_code\\((\\d+)\\)/ && print $1')
                if [[ \"$commit_msg\" =~ {block_tag} ]]; then
                    if [[ -n \"$exit_code\" ]]; then
                        trap \"exit $exit_code\" SIGINT
                    fi
                    # sleep is not a builtin so we won't handle SIGINT while
                    # that's running. Hack suggested by ChatGPT: just spawn it
                    # then use wait, which is a builtin.
                    sleep infinity &
                    wait $!
                fi
                # Extract the exit code and pass it to exit if there is one, otherwise pass 0.
                exit ${{exit_code:-0}}
                ",
                pid_path_prefix = dir.path().join(Self::PID_FILENAME_PREFIX),
                siginted_path_prefix = dir.path().join(Self::SIGINTED_FILENAME_PREFIX),
                lock_filename = if use_lockfile { Self::LOCK_FILENAME } else { "" },
                bug_detected_path = dir.path().join(Self::BUG_DETECTED_PATH),
                block_tag = Self::BLOCK_COMMIT_MSG_TAG,
            );

            Self {
                dir,
                test_name,
                script: script.into(),
            }
        }

        // Pass this to Manager::new
        pub fn program(&self) -> OsString {
            "bash".into()
        }
        // Pass this to Manager::new
        pub fn args(&self) -> Vec<OsString> {
            vec!["-xc".into(), self.script.clone()]
        }

        // Path used by the running script to signal an event.
        fn signalling_path(&self, filename_prefix: &str, hash: &CommitHash) -> PathBuf {
            // Argh I dunno this is annoying.
            let mut filename = OsString::from(filename_prefix);
            filename.push(hash);
            self.dir.path().join(filename)
        }

        // If this path exists, two instances of the script used the same worktree at once.
        fn bug_detected_path(&self) -> PathBuf {
            self.dir.path().join(Self::BUG_DETECTED_PATH)
        }

        // Has the script been started so far for this test?
        pub fn was_started(&self, hash: &CommitHash) -> bool {
            self.signalling_path(Self::PID_FILENAME_PREFIX, hash)
                .exists()
        }

        // Blocks until the script is started for the given commit hash.
        pub async fn started(&self, hash: &CommitHash) -> StartedTestScript {
            let pid_path = self.signalling_path(Self::PID_FILENAME_PREFIX, hash);
            path_exists(&pid_path).await;
            let content = fs::read_to_string(pid_path).expect("couldn't read PID file");
            // The script appends a PID to the file each time, this is handy for num_runs.
            // So we just take the last one.
            let line = content
                .trim()
                .split("\n")
                .last()
                .expect("PID file existed but contained no PIDs");
            StartedTestScript {
                script: self,
                hash: hash.to_owned(),
                pid: Pid::from_raw(line.parse().unwrap_or_else(|_| {
                    panic!("couldn't parse PID file as integer (content: {content:?}")
                })),
            }
        }

        // Number of times the script has been successfully spawned for this
        // commit, since StartedTestScript::reset_started was called for it.
        pub fn num_runs(&self, hash: &CommitHash) -> usize {
            let path = self.signalling_path(Self::PID_FILENAME_PREFIX, hash);
            if !path.exists() {
                return 0;
            }
            // Awkward: it's probably still possible to return 0 here, we can
            // probably observe the file whe it exists but doesn't have any content yet.
            io::BufReader::new(File::open(path).expect("couldn't open TestScript signalling file"))
                .lines()
                .count()
        }

        pub fn as_test(
            &self,
            cache_policy: CachePolicy,
            needs_worktree: bool,
            depends_on: impl IntoIterator<Item = TestName>,
        ) -> Test {
            Test {
                name: self.test_name.clone(),
                program: self.program(),
                args: self.args(),
                needs_resources: if needs_worktree {
                    [(ResourceKey::Worktree, 1)].into()
                } else {
                    [].into()
                },
                shutdown_grace_period: Duration::from_secs(5),
                cache_policy,
                config_hash: 0,
                depends_on: depends_on.into_iter().collect(),
            }
        }
    }

    // Hack to check for stuff that is orthogonal to any particular test, so we
    // don't wanna have to it in every individual test.
    impl Drop for TestScript {
        fn drop(&mut self) {
            let path = self.bug_detected_path();
            if path.exists() {
                let content =
                    std::fs::read_to_string(path).expect("couldn't read bug-detected path");
                let msg = format!("The test script detected one or more bugs: {}", content);
                if panicking() {
                    // If you panic during a panic (i.e. if this fails when the test had already
                    // failed) you get a huge splat. Just log instead.
                    error!("{}", msg);
                } else {
                    panic!("{}", msg);
                }
            }
        }
    }

    // Like a TestScript, but you can only get one once it's already startd running, so it has extra
    // operations.
    struct StartedTestScript<'a> {
        script: &'a TestScript,
        hash: CommitHash,
        pid: Pid,
    }

    impl<'a> StartedTestScript<'a> {
        // Blocks until the script has received a SIGINT.
        pub async fn siginted(&self) {
            path_exists(
                self.script
                    .signalling_path(TestScript::SIGINTED_FILENAME_PREFIX, &self.hash),
            )
            .await;
        }

        // SIGTERM the instance of the script. Use this if you want the script
        // to "fail with an error". This preferable to SIGKILL because that will
        // prevent the underlying script from performing its cleanup.
        pub fn sigterm(&self) {
            kill(self.pid, Signal::SIGTERM).expect("couldn't SIGKILL test script");
        }

        // Forget "started" state so that TestScript::started can usefully be called again.
        pub fn reset_started(self) {
            remove_file(
                self.script
                    .signalling_path(TestScript::PID_FILENAME_PREFIX, &self.hash),
            )
            .expect("couldn't delete test script PID file");
        }
    }

    fn dump_want_statuses(want: &HashMap<TestCaseId, (TestCase, VecDeque<TestStatus>)>) -> String {
        let mut ret = String::from("");
        for (test_case, statuses) in want.values() {
            ret.push_str(&format!("{:?}\n", test_case));
            for status in statuses {
                ret.push_str(&format!("\t{:?}\n", status));
            }
        }
        ret
    }
    // Expect the series of notifications provided for each test case.
    // case. Also assert that the necessary precursor notifications arrive.
    // Panics if any of the input series are empty.
    async fn expect_notifs_10s(
        results: &mut broadcast::Receiver<Arc<Notification>>,
        want: impl IntoIterator<Item = (TestCase, VecDeque<TestStatus>)>,
    ) -> anyhow::Result<()> {
        let timeout = Instant::now() + Duration::from_secs(10);
        let mut want: HashMap<TestCaseId, _> = want
            .into_iter()
            .map(|(test_case, statuses)| (test_case.id(), (test_case, statuses)))
            .collect();
        while !want.is_empty() {
            let notif = select!(
                _ = sleep_until(timeout) => {
                    bail!("timeout after 5s, remaining results:\n{}",
                        dump_want_statuses(&want));
                },
                output = results.recv() => {
                    output.context(format!(
                        "test result stream terminated, remaining results:\n{}",
                        dump_want_statuses(&want)))?
                }
            );
            let (_tc, want_statuses) = want.get_mut(&notif.test_case.id()).context(format!(
                "got notification for unexpected test case: {notif:?}",
            ))?;
            let want_status = want_statuses.pop_front().expect("empty status series");
            if want_statuses.is_empty() {
                want.remove(&notif.test_case.id());
            }
            if notif.status != want_status {
                bail!(
                    "unexpected test notification for {:?}, got {:?} want {:?}",
                    notif.test_case,
                    notif.status,
                    want_status
                );
            }
        }
        Ok(())
    }

    async fn expect_no_more_results(
        results: &mut broadcast::Receiver<Arc<Notification>>,
        m: &Manager<TempRepo>,
    ) -> anyhow::Result<()> {
        select!(
            _ = sleep(Duration::from_secs(1)) => bail!("didn't settle after 1s"),
            result = results.recv() => bail!("unexpected test result received: {:?}", result),
            _ = m.settled() => Ok(())
        )
    }

    struct TestScriptFixture {
        _db_dir: TempDir,
        repo: Arc<TempRepo>,
        scripts: Vec<TestScript>,
        manager: Manager<TempRepo>,
    }

    struct TestScriptFixtureBuilder {
        num_worktrees: usize,
        num_tests: usize,
        // If we end up with more awkward "configuration vectors" like these
        // then probably we should just switch this builder over to constructing
        // via config::Test::parse.
        cache_policies: Vec<CachePolicy>,
        needs_worktree: Vec<bool>,
        dependencies: Vec<(usize, usize)>,
    }

    impl TestScriptFixtureBuilder {
        pub fn num_worktrees(mut self, n: usize) -> Self {
            self.num_worktrees = n;
            self
        }

        // Ensure we are prepared to set up at least n tests.
        fn extend(mut self, n: usize) -> Self {
            while self.cache_policies.len() < n {
                self.cache_policies.push(CachePolicy::ByCommit);
            }
            while self.needs_worktree.len() < n {
                self.needs_worktree.push(true);
            }
            self
        }

        // Tests per commit.
        pub fn num_tests(mut self, n: usize) -> Self {
            self.num_tests = n;
            self.extend(n)
        }

        // cache_policies[i] will be the cache policy for the ith test.
        pub fn cache_policies(mut self, pols: impl IntoIterator<Item = CachePolicy>) -> Self {
            self.cache_policies = pols.into_iter().collect();
            let len = self.cache_policies.len();
            self.num_tests(len)
        }

        // needs_worktrees[i] will decide if the ith test requires a worktree.
        pub fn needs_worktree(mut self, needs_worktree: impl IntoIterator<Item = bool>) -> Self {
            self.needs_worktree = needs_worktree.into_iter().collect();
            let len = self.needs_worktree.len();
            self.num_tests(len)
        }

        // Declare pairs of text indexes where the first depends on the second.
        pub fn dependencies(mut self, deps: impl IntoIterator<Item = (usize, usize)>) -> Self {
            self.dependencies = deps.into_iter().collect();
            let max_idx = self
                .dependencies
                .iter()
                .map(|&(x, y)| max(x, y))
                .max()
                .unwrap_or(0);
            self.extend(max_idx + 1)
        }
    }

    async fn nonempty_temp_repo() -> Arc<TempRepo> {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        // TODO: We need to have a commit in the repo otherwise manager
        // setup will fail. This is a bug.
        repo.commit("hello,", some_time())
            .await
            .expect("couldn't create base commit");
        repo
    }

    impl TestScriptFixtureBuilder {
        pub async fn build(&self) -> TestScriptFixture {
            let repo = nonempty_temp_repo().await;
            let scripts: Vec<TestScript> = (0..self.num_tests)
                // Here we pass needs_worktree[i] to configure whether the script should
                // try to detect sharing a worktree with another test run. If it
                // doesn't a worktree then that sharing is harmless and
                // expected.
                .map(|i| {
                    TestScript::new(TestName::new(format!("test_{i}")), self.needs_worktree[i])
                })
                .collect();
            let db_dir = TempDir::new().expect("couldn't make temp dir for result DB");
            let tests = izip!(
                0..self.num_tests,
                &scripts,
                &self.cache_policies,
                &self.needs_worktree
            )
            .map(|(i, script, &cache_policy, &needs_worktree)| {
                let dep_names = self
                    .dependencies
                    .iter()
                    .filter(|(from_idx, _)| *from_idx == i)
                    .map(|(_, to_idx)| TestName::new(format!("test_{to_idx}")));
                script.as_test(cache_policy, needs_worktree, dep_names)
            });
            let manager = Manager::builder(
                repo.clone(),
                Database::create_or_open(db_dir.path()).expect("couldn't setup result DB"),
                tests,
                [],
            )
            .num_worktrees(self.num_worktrees)
            .build()
            .await
            .expect("couldn't set up manager");
            TestScriptFixture {
                manager,
                scripts,
                repo,
                _db_dir: db_dir,
            }
        }
    }

    impl TestScriptFixture {
        pub fn builder() -> TestScriptFixtureBuilder {
            TestScriptFixtureBuilder {
                num_worktrees: 2,
                num_tests: 2,
                cache_policies: vec![CachePolicy::ByCommit; 2],
                needs_worktree: vec![true; 2],
                dependencies: vec![],
            }
        }

        // Convenience helper to construct a TestCase referring to this fixture's configuration.
        async fn test_case(&self, hash: impl Borrow<CommitHash>, test_idx: usize) -> TestCase {
            TestCase::new(
                hash.borrow().to_owned(),
                self.manager.tests[test_idx].clone(),
                self.repo.as_ref(),
            )
            .await
            .unwrap()
        }
    }

    #[test_log::test(tokio::test)]
    async fn should_run_single() {
        let mut f = TestScriptFixture::builder().num_tests(1).build().await;
        let mut results = f.manager.results();
        let hash = f
            .repo
            .commit("hello,", some_time())
            .await
            .expect("couldn't create test commit");
        f.manager.set_revisions(vec![hash.clone()]).await.unwrap();
        // We should get a singular result because we only fed in one revision.
        expect_notifs_10s(
            &mut results,
            [(
                f.test_case(&hash, 0).await,
                vec![
                    TestStatus::Enqueued,
                    TestStatus::Started,
                    TestStatus::Completed(TestResult { exit_code: 0 }),
                ]
                .into(),
            )],
        )
        .await
        .expect("bad test result");
        expect_no_more_results(&mut results, &f.manager)
            .await
            .unwrap()
    }

    #[test_log::test(tokio::test)]
    async fn should_cancel_running() {
        let mut f = TestScriptFixture::builder().num_tests(2).build().await;
        // First commit's test will block forever.
        let hash1 = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
            .await
            .expect("couldn't create test commit");
        let mut results = f.manager.results();
        f.manager.set_revisions(vec![hash1.clone()]).await.unwrap();
        let started_hash1 = timeout_5s(join_all(f.scripts.iter().map(|s| s.started(&hash1))))
            .await
            .expect("not all scripts run for hash1");
        // Second commit's test will terminate quickly.
        let hash2 = f
            .repo
            .commit("hello,", some_time())
            .await
            .expect("couldn't create test commit");
        f.manager.set_revisions(vec![hash2.clone()]).await.unwrap();
        timeout_5s(f.scripts[0].started(&hash2))
            .await
            .expect("f.scripts[0] did not run for hash2");
        timeout_5s(join_all(
            started_hash1
                .iter()
                .map(|s: &StartedTestScript| s.siginted()),
        ))
        .await
        .expect("hash1 tests did not all get siginted");
        expect_notifs_10s(
            &mut results,
            // awu weh, weh mah
            [
                (
                    f.test_case(&hash1, 0).await,
                    vec![
                        TestStatus::Enqueued,
                        TestStatus::Started,
                        TestStatus::Canceled,
                    ]
                    .into(),
                ),
                (
                    f.test_case(&hash1, 1).await,
                    vec![
                        TestStatus::Enqueued,
                        TestStatus::Started,
                        TestStatus::Canceled,
                    ]
                    .into(),
                ),
                // This isn't what we're testing here but we need to assert that it comes in so we can
                // check below that nothing else comes in.
                (
                    f.test_case(&hash2, 0).await,
                    vec![
                        TestStatus::Enqueued,
                        TestStatus::Started,
                        TestStatus::Completed(TestResult { exit_code: 0 }),
                    ]
                    .into(),
                ),
                (
                    f.test_case(&hash2, 1).await,
                    vec![
                        TestStatus::Enqueued,
                        TestStatus::Started,
                        TestStatus::Completed(TestResult { exit_code: 0 }),
                    ]
                    .into(),
                ),
            ],
        )
        .await
        .unwrap();
        expect_no_more_results(&mut results, &f.manager)
            .await
            .unwrap()
    }

    // This is not actually testing functionality, this is a meta-test, yikes this is
    // over-engineered.
    #[test_log::test(tokio::test)]
    async fn should_not_settle() {
        let mut f = TestScriptFixture::builder().num_tests(1).build().await;
        // First commit's test will block forever.
        let hash = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
            .await
            .expect("couldn't create test commit");
        f.manager.set_revisions([hash.clone()]).await.unwrap();
        timeout_5s(f.scripts[0].started(&hash))
            .await
            .expect("script did not start");
        select!(
            _ = sleep(Duration::from_secs(1)) => (),
            _ = f.manager.settled() => panic!("manager settled unexpectedly"),
        )
    }

    #[test_log::test(tokio::test)]
    async fn should_cache_results() {
        let mut f = TestScriptFixture::builder()
            .cache_policies([
                CachePolicy::NoCaching,
                CachePolicy::ByCommit,
                CachePolicy::ByTree,
            ])
            .build()
            .await;
        f.repo
            .commit("yarp", some_time())
            .await
            .expect("couldn't create test commit");

        // Set up two commits to test. Note this is a bit of an odd test case.
        // The real world case we are thinking of here is probably swiching
        // between two branches with a common base rather than two ranges with
        // an ancestry relation. But to do that we'd need more helpers in our
        // Git library. So we just take advantage of our knowledge that this
        // weirdness of the test case is irrelevant given how the implementation
        // works (it doesn't know about the structure of the history).
        let orig_hash = f
            .repo
            .commit("yarp", some_time())
            .await
            .expect("couldn't create test commit");
        // This one has a different commit hash but the same tree.
        let same_tree = f
            .repo
            .commit("darp", some_time())
            .await
            .expect("couldn't create test commit");

        // Test the first commit and wait for it to be complete.
        f.manager
            .set_revisions(vec![orig_hash.clone()])
            .await
            .unwrap();
        f.manager.settled().await;
        // Sanity check that the scripts actually got run.
        assert_eq!(f.scripts[0].num_runs(&orig_hash), 1);
        assert_eq!(f.scripts[1].num_runs(&orig_hash), 1);
        assert_eq!(f.scripts[2].num_runs(&orig_hash), 1);

        f.manager
            .set_revisions(vec![same_tree.clone()])
            .await
            .unwrap();
        f.manager.settled().await;
        // Without caching the script should get run every time.
        assert_eq!(f.scripts[0].num_runs(&same_tree), 1);
        // Commit has has changed so new commit should get retested for ByCommit.
        assert_eq!(f.scripts[1].num_runs(&same_tree), 1);
        // But not for ByTree
        assert_eq!(f.scripts[2].num_runs(&same_tree), 0);

        f.manager
            .set_revisions(vec![orig_hash.clone()])
            .await
            .unwrap();
        f.manager.settled().await;
        assert_eq!(f.scripts[0].num_runs(&orig_hash), 2);
        assert_eq!(f.scripts[1].num_runs(&orig_hash), 1);
        assert_eq!(f.scripts[2].num_runs(&orig_hash), 1);
    }

    #[test_case(1, 1 ; "single worktree, one test")]
    #[test_case(4, 1 ; "multiple worktrees, one test")]
    #[test_case(4, 4 ; "multiple worktrees, multiple tests")]
    #[test_log::test(tokio::test)]
    async fn should_handle_many(num_worktrees: usize, num_tests: usize) {
        let mut f = TestScriptFixture::builder()
            .num_tests(num_tests)
            .num_worktrees(num_worktrees)
            .build()
            .await;
        let mut hashes = Vec::new();
        let mut want_results = Vec::new();
        for i in 0..50 {
            let hash = f
                .repo
                .commit(TestScript::exit_code_tag(i as u32), some_time())
                .await
                .expect("couldn't create test commit");
            for j in 0..num_tests {
                want_results.push((
                    f.test_case(&hash, j).await,
                    vec![
                        TestStatus::Enqueued,
                        TestStatus::Started,
                        TestStatus::Completed(TestResult { exit_code: i }),
                    ]
                    .into(),
                ));
            }
            hashes.push(hash);
        }
        let mut results = f.manager.results();
        f.manager.set_revisions(hashes.clone()).await.unwrap();
        expect_notifs_10s(&mut results, want_results)
            .await
            .expect("bad results");
    }

    #[test_log::test(tokio::test)]
    async fn should_respect_resource_limits() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let mut hashes = Vec::new();
        for _ in 0..10 {
            hashes.push(
                repo.commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
                    .await
                    .expect("couldn't create test commit"),
            );
        }
        let script = TestScript::new(TestName::new("my_test"), true);
        // We only have 2 tokens
        let resource_tokens = HashMap::from([(
            ResourceKey::UserToken("foo".into()),
            vec!["foo1".into(), "foo2".into()],
        )]);
        // And a test that requires one of those tokens.
        let tests = [Test {
            name: TestName::new("my_test"),
            program: script.program(),
            args: script.args(),
            needs_resources: HashMap::from([
                (ResourceKey::Worktree, 1),
                (ResourceKey::UserToken("foo".into()), 1),
            ]),
            shutdown_grace_period: Duration::from_secs(5),
            cache_policy: CachePolicy::ByCommit,
            config_hash: 0,
            depends_on: vec![],
        }];
        let db_dir = TempDir::new().expect("couldn't make temp dir for result DB");
        let mut m = Manager::builder(
            repo.clone(),
            Database::create_or_open(db_dir.path()).expect("couldn't setup result DB"),
            tests,
            resource_tokens,
        )
        .num_worktrees(4)
        .build()
        .await
        .expect("couldn't set up manager");
        m.set_revisions(hashes.clone()).await.unwrap();

        let mut start_futs = hashes.iter().map(|h| Box::pin(script.started(h))).collect();
        for _ in 0..2 {
            let (_started, _index, remaining) = timeout_5s(select_all(start_futs))
                .await
                .expect("didn't start first two jobs");
            start_futs = remaining;
        }

        // Ugh, dunno how to do this except just wait for 1s...
        select!(
            _ = sleep(Duration::from_secs(1)) => (), // OK, nothing else ran.
            _ = select_all(start_futs) => panic!("extra jobs started, resource limits not respected"),
        )
    }

    #[test_log::test(tokio::test)]
    async fn test_job_env() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let hash = repo
            .commit("hello,", some_time())
            .await
            .expect("couldn't create test commit");
        let db_dir = TempDir::new().expect("couldn't make temp dir for result DB");
        let mut m = Manager::builder(
            repo.clone(),
            Database::create_or_open(db_dir.path()).expect("couldn't setup result DB"),
            [Test {
                name: TestName::new("my_test"),
                program: OsString::from("bash"),
                args: vec![
                    "-c".into(),
                    OsString::from(format!("env >> {0:?}/env.txt", temp_dir.path())),
                ],
                needs_resources: [
                    (ResourceKey::Worktree, 1),
                    (ResourceKey::UserToken("my_resource".into()), 2),
                ]
                .into(),
                shutdown_grace_period: Duration::from_secs(5),
                cache_policy: CachePolicy::ByCommit,
                config_hash: 0,
                depends_on: vec![],
            }],
            [
                (
                    ResourceKey::UserToken("my_resource".into()),
                    vec!["thing1".into(), "thing2".into(), "thing3".into()],
                ),
                (
                    ResourceKey::UserToken("other_resource".into()),
                    vec!["whing1".into(), "whing2".into(), "whing3".into()],
                ),
            ],
        )
        .build()
        .await
        .expect("couldn't set up manager");

        m.set_revisions([hash.clone()])
            .await
            .expect("set_revisions failed");
        m.settled().await;

        let env_dump = fs::read_to_string(temp_dir.path().join("env.txt"))
            .expect("couldn't read env dumped from test script");
        let env: HashMap<&str, &str> = env_dump
            .trim()
            .split("\n")
            .filter_map(|line| {
                let parts: Vec<_> = line.splitn(2, "=").collect();
                if parts.len() != 2 {
                    return None;
                }
                Some((parts[0], parts[1]))
            })
            .collect();
        assert_eq!(
            env.get("LCI_ORIGIN"),
            // Ugh I don't fucking know, as_ref as_ref as_ref as_ref just deal
            // with it this is how we write Rust this is good Rust as_ref as_ref.
            Some(repo.path().to_string_lossy().as_ref()).as_ref()
        );
        assert_eq!(
            env.get("LCI_COMMIT").map(|t| CommitHash::new(*t)),
            Some(hash)
        );
        let resource0 = env
            .get("LCI_RESOURCE_my_resource_0")
            .expect("didn't get resource0");
        assert!(
            resource0.starts_with("thing"),
            "bad resource 0: {resource0:?}'"
        );
        let resource1 = env
            .get("LCI_RESOURCE_my_resource_1")
            .expect("didn't get resource1");
        assert!(
            resource1.starts_with("thing"),
            "bad resource 2: {resource1:?}'"
        );
        assert_eq!(env.get("LCI_RESOURCE_my_resource_2"), None);
        assert_eq!(env.get("LCI_RESOURCE_other_resource_0"), None);
    }

    #[test_log::test(tokio::test)]
    async fn should_not_start_canceled() {
        let mut f = TestScriptFixture::builder()
            .num_tests(1)
            .num_worktrees(2)
            .build()
            .await;
        let mut results = f.manager.results();
        let mut hashes = Vec::new();
        for _ in 0..5 {
            hashes.push(
                f.repo
                    .commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
                    .await
                    .expect("couldn't create test commit"),
            );
        }

        // We're gonna start one test and have another become blocked waiting
        // for a worktree. In order to make it deterministic which one gets
        // blocked, we'll do this in two phases.
        f.manager
            .set_revisions(vec![hashes[0].clone()])
            .await
            .unwrap();
        // wait for first test to get started.
        expect_notifs_10s(
            &mut results,
            [(
                f.test_case(&hashes[0], 0).await,
                vec![TestStatus::Enqueued, TestStatus::Started].into(),
            )],
        )
        .await
        .expect("bad test result");

        // Now we enqueue the test that should block.
        f.manager
            .set_revisions(vec![hashes[0].clone(), hashes[1].clone()])
            .await
            .unwrap();
        expect_notifs_10s(
            &mut results,
            [(
                f.test_case(&hashes[1], 0).await,
                vec![TestStatus::Enqueued].into(),
            )],
        )
        .await
        .expect("bad test result");

        // Now we cancel both of those tests.
        f.manager.set_revisions(Vec::new()).await.unwrap();
        expect_notifs_10s(
            &mut results,
            [(
                f.test_case(&hashes[0], 0).await,
                vec![TestStatus::Canceled].into(),
            )],
        )
        .await
        .expect("bad test result");

        expect_no_more_results(&mut results, &f.manager)
            .await
            .unwrap();

        assert!(!f.scripts[0].was_started(&hashes[1]));
    }

    #[test_log::test(tokio::test)]
    async fn should_not_cache() {
        let mut f = TestScriptFixture::builder()
            .num_tests(2)
            .num_worktrees(4)
            .build()
            .await;
        let mut results = f.manager.results();

        // This commit's tests will be terminated by SIGINT if they receive it,
        // which is "an error"
        let hash_error = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
            .await
            .expect("couldn't create test commit");
        // This commit's tests will shut down with an error exit-code if
        // SIGINTED which is normally a test failure. But this should not be
        // cached if the SIGINT was due to the job being canceled.
        let mut commit_msg = OsString::from(TestScript::BLOCK_COMMIT_MSG_TAG);
        commit_msg.push(TestScript::exit_code_tag(1));
        let hash_fail = f
            .repo
            .commit(commit_msg, some_time())
            .await
            .expect("couldn't create test commit");

        // Wait until all tests are started.
        f.manager
            .set_revisions(vec![hash_error.clone(), hash_fail.clone()].clone())
            .await
            .unwrap();
        expect_notifs_10s(
            &mut results,
            [
                (
                    f.test_case(&hash_error, 0).await,
                    vec![TestStatus::Enqueued, TestStatus::Started].into(),
                ),
                (
                    f.test_case(&hash_error, 1).await,
                    vec![TestStatus::Enqueued, TestStatus::Started].into(),
                ),
                (
                    f.test_case(&hash_fail, 0).await,
                    vec![TestStatus::Enqueued, TestStatus::Started].into(),
                ),
                (
                    f.test_case(&hash_fail, 1).await,
                    vec![TestStatus::Enqueued, TestStatus::Started].into(),
                ),
            ],
        )
        .await
        .expect("bad test result");

        // Cause one to fail with an error. We take advantage of the fact that
        // this whole tool considers it an "error" when a test exits with a
        // signal instead of exiting with a nonzero code.
        let started_script = f.scripts[0].started(&hash_error).await;
        started_script.sigterm();
        expect_notifs_10s(
            &mut results,
            [(
                f.test_case(&hash_error, 0).await,
                vec![TestStatus::Error(String::from("terminated by signal 15"))].into(),
            )],
        )
        .await
        .expect("didn't get error after killing script");
        started_script.reset_started();

        // ... and the others to be canceled.
        f.manager.set_revisions(vec![]).await.unwrap();
        expect_notifs_10s(
            &mut results,
            [
                (
                    f.test_case(&hash_error, 1).await,
                    vec![TestStatus::Canceled].into(),
                ),
                (
                    f.test_case(&hash_fail, 0).await,
                    vec![TestStatus::Canceled].into(),
                ),
                (
                    f.test_case(&hash_fail, 1).await,
                    vec![TestStatus::Canceled].into(),
                ),
            ],
        )
        .await
        .expect("didn't see test cancellation");

        // They should now get run a second time. i.e. none of the results should have got hashed.
        f.manager
            .set_revisions(vec![hash_error.clone(), hash_fail.clone()].clone())
            .await
            .unwrap();
        select!(
            _ = sleep(Duration::from_secs(5)) => panic!("error'd test not re-run"),
            _ = f.scripts[0].started(&hash_error) => ()
        );
        select!(
            _ = sleep(Duration::from_secs(5)) => panic!("canceled test not re-run"),
            _ = f.scripts[1].started(&hash_error) => ()
        );
        select!(
            _ = sleep(Duration::from_secs(5)) => panic!("error'd test not re-run"),
            _ = f.scripts[0].started(&hash_fail) => ()
        );
        select!(
            _ = sleep(Duration::from_secs(5)) => panic!("canceled test not re-run"),
            _ = f.scripts[1].started(&hash_fail) => ()
        );
    }

    #[test_log::test(tokio::test)]
    async fn should_not_require_worktree() {
        let mut f = TestScriptFixture::builder()
            .needs_worktree([false, false, false])
            .num_worktrees(1)
            .build()
            .await;
        let test_hash = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
            .await
            .expect("couldn't create test commit");
        let head_hash = f
            .repo
            .commit("woodly doodly", some_time())
            .await
            .expect("couldn't create test commit");
        f.manager
            .set_revisions(vec![test_hash.clone()])
            .await
            .unwrap();
        // Even though we have three tests that never finish, and only one
        // worktree, they should all start.
        timeout_5s(join_all(f.scripts.iter().map(|s| s.started(&test_hash))))
            .await
            .expect("not all scripts run for hash");
        // We were testing test_hash but we shouldn't have checked it out, since
        // we don't "own" the worktree.
        assert_eq!(f.repo.rev_parse("HEAD").await.unwrap(), Some(head_hash));
    }

    #[test_log::test(tokio::test)]
    async fn should_detect_dependency_loops() {
        // Ummmmmmmmmmmmmmm I can't be bothered to write more test cases lmao
        // algorithms are boring.
        let tests = [
            Test {
                name: TestName::new("my_test"),
                program: "foo".into(),
                args: vec!["bar".into()],
                needs_resources: HashMap::new(),
                shutdown_grace_period: Duration::from_secs(5),
                cache_policy: CachePolicy::ByCommit,
                config_hash: 0,
                depends_on: vec![TestName::new("my_other_test")],
            },
            Test {
                name: TestName::new("my_other_test"),
                program: "foo".into(),
                args: vec!["bar".into()],
                needs_resources: HashMap::new(),
                shutdown_grace_period: Duration::from_secs(5),
                cache_policy: CachePolicy::ByCommit,
                config_hash: 0,
                depends_on: vec![TestName::new("my_test")],
            },
        ];
        let repo = nonempty_temp_repo().await;
        let db_dir = TempDir::new().expect("couldn't make temp dir for result DB");
        let result = Manager::builder(
            repo.clone(),
            Database::create_or_open(db_dir.path()).expect("couldn't setup result DB"),
            tests,
            [],
        )
        .build()
        .await;
        match result {
            Ok(_) => panic!("Successfully created Manager with test dep cycles, should fail"),
            // Hacky lazy attempt to detect if this test got broken and we are
            // failing for the wrong reason.
            Err(err) => {
                if !err.to_string().to_lowercase().contains("cycle") {
                    panic!("Didn't see the word 'cycle' in error message, is this test working? error: {:?}", err);
                }
            }
        }
    }

    #[test_log::test(tokio::test)]
    async fn should_wait_for_dependencies() {
        let mut f = TestScriptFixture::builder()
            .dependencies([(1, 0)])
            .num_worktrees(2)
            .build()
            .await;
        let hash = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
            .await
            .unwrap();
        f.manager.set_revisions(vec![hash.clone()]).await.unwrap();
        timeout_5s(f.scripts[0].started(&hash))
            .await
            .expect("Initial test did not start");

        // Annoying: now we need to check that the second test doesn't get
        // started. But how long do we wait...? Just, some arbitrary time. So
        // sometimes this test can pass even if there's a bug :(
        sleep(Duration::from_secs(1)).await;

        assert!(
            !f.scripts[1].was_started(&hash),
            "Test with dependency started when dep not finished"
        );

        // TODO: Test when dependency fails.
        // TODO: Test that changes to dependency config hashes invalidates
        // result cache.
    }

    // TODO: if the tests fail, the TempWorktree cleanup goes haywire, something
    // to do with panic and drop order I think.
}
