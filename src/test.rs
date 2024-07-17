use core::fmt;
use core::fmt::Display;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::pin::pin;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use futures::future::{self, try_join_all, Either};
use log::debug;
use log::error;
use log::info;
use nix::sys::signal::kill;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::git::PersistentWorktree;
use crate::git::TempWorktree;
use crate::git::{CommitHash, Worktree};
use crate::process::OutputExt;
use crate::resource::Pools;

// A test task that will need to be repeated for each commit.
pub struct Test {
    pub name: String,
    pub program: OsString,
    pub args: Vec<OsString>,
    // Indexes of pools in the Manager's token_pools from which this test needs
    // a resource-token before it can begin.
    pub needs_resource_idxs: Vec<usize>,
}

impl Test {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        cmd
    }
}

impl Display for Test {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<test: {:?}>", self.name)
    }
}

#[derive(Default)]
pub struct ManagerBuilder<W> {
    // This needs to be an Arc because we hold onto a reference to it for a
    // while, and create temporary worktrees from it in the background.
    repo: Arc<W>,
    tests: Vec<Test>,
    token_pool_sizes: Vec<usize>,

    num_worktrees: usize,
    worktree_prefix: String,
    worktree_dir: PathBuf,
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

    // Starts the workers. You must call close() before dropping it.
    //
    // TODO: This doesn't work if there are no commits in the repository. Not sure I care about
    // this, but the solution would be to create the worktrees ondemand, when we have a revision we
    // are actually trying to test. That might be a good idea anyway, so probably it's preferable to
    // just do that for its own sake and leave the empty-repo problem as a nice freebie.
    pub async fn build(self) -> anyhow::Result<Manager>
    where
        // We need to specify 'static here. Just because we have an Arc over the
        // repo that doesn't mean it automatically satisfies 'static:
        // https://users.rust-lang.org/t/why-is-t-static-constrained-when-using-arc-t-and-thread-spawn/26262/2
        // It would be much more convenient to just specify some or all these
        // trait bounds as subtraits of Workrtree. But I dunno, that feels Wrong.
        W: Worktree + Sync + Send + 'static,
    {
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
            debug!("Creating worktree: {:?}", t.path());
            TempWorktree::new::<W>(self.repo.borrow(), t).await
        }))
        .await
        .context("setting up temporary worktrees")?;
        info!("Worktree setup done.");
        let (result_tx, _) = broadcast::channel(32);
        Ok(Manager {
            result_tx,
            job_cts: HashMap::new(),
            job_counter: JobCounter::new(),
            tests: self.tests.into_iter().map(Arc::new).collect(),
            resource_pools: Arc::new(Pools::new(self.token_pool_sizes, worktrees)),
        })
    }
}

// Manages a bunch of worker threads that run tests for the current set of revisions.
pub struct Manager {
    job_cts: HashMap<CommitHash, CancellationToken>,
    job_counter: JobCounter,
    result_tx: broadcast::Sender<Arc<TestResult>>,
    tests: Vec<Arc<Test>>,
    // Pools contains sets of intangible arbitrary "resources" that can be used to throttle test
    // jobs, and also tracks access to reused worktrees. The indices of the token-type resources
    // will be referenced by Test::needs_resource_idx values.
    resource_pools: Arc<Pools<TempWorktree>>,
}

impl Manager {
    pub fn builder<W, I: IntoIterator<Item = Test>, J: IntoIterator<Item = usize>>(
        // This needs to be an Arc because we hold onto a reference to it for a
        // while, and create temporary worktrees from it in the background.
        repo: Arc<W>,
        tests: I,
        token_pool_sizes: J,
    ) -> ManagerBuilder<W> {
        ManagerBuilder {
            repo,
            tests: tests.into_iter().collect(),
            token_pool_sizes: token_pool_sizes.into_iter().collect(),

            num_worktrees: 1,
            worktree_prefix: "worktree-".to_owned(),
            worktree_dir: env::temp_dir(),
        }
    }

    // Interrupt any revisions that are not in revs, start testing all revisions in revs that are
    // not already tested or being tested.
    // It doesn't make sense to call this function if you don't have a receiver
    // from already having called [[results]].
    pub fn set_revisions<I: IntoIterator<Item = CommitHash>>(&mut self, revs: I) {
        let mut to_start = HashSet::<CommitHash>::from_iter(revs);
        let mut cancel_revs = Vec::new();
        for rev in self.job_cts.keys() {
            // We're already testing rev, so we don't need to kick it off below.
            if !to_start.remove(rev) {
                // This rev is being tested but wasn't in rev_set.
                cancel_revs.push(rev.clone())
            }
        }
        info!("Starting {:?}, cancelling {:?}", to_start, cancel_revs);
        for rev in cancel_revs {
            self.job_cts[&rev].cancel();
            self.job_cts.remove(&rev);
        }

        for rev in to_start {
            for test in self.tests.iter() {
                let ct = CancellationToken::new();
                self.job_cts.insert(rev.to_owned(), ct.clone());
                let job = TestJob {
                    ct,
                    test: test.clone(),
                    rev: rev.to_owned(),
                    _token: self.job_counter.get(),
                };
                let pools = self.resource_pools.clone();
                let tx = self.result_tx.clone();
                tokio::spawn(async move {
                    let resources = pools.get(job.test.needs_resource_idxs.clone()).await;
                    let worktree = resources.obj();
                    let result = job.run(worktree).await;
                    // Note: must not drop test until the send is complete, or we would break
                    // settled().
                    let _ = tx.send(Arc::new(TestResult {
                        hash: job.rev,
                        test_name: job.test.name.to_owned(),
                        result
                    }))
                    .map_err(|e|
                        error!("Dropping a result. Seems nobody is listening to Manager::results(): {}", e)
                    );
                });
            }
        }
    }

    // Streams results back. Note you need to call this _before_ you generate the results you want
    // to receive.
    //
    // I think the "proper" solution for this is to return a Stream. But I don't understand it.
    pub fn results(&self) -> broadcast::Receiver<Arc<TestResult>> {
        self.result_tx.subscribe()
    }

    // Completes once there are no pending jobs or results.
    pub async fn settled(&self) {
        self.job_counter.zero().await;
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.set_revisions([]);
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
    // TODO: Unclear if there's a way to avoid the atomic operations incurred by cloning these Arcs.
    // There is no builtin equivalent to thread::scope for async. If we had that, maybe it would
    // become possible to convince the compiler that the Manager outlives its Tests. Not sure.
    test: Arc<Test>,
    rev: CommitHash,
    _token: JobToken,
}

impl TestJob {
    async fn run<W>(&self, worktree: &W) -> anyhow::Result<TestOutcome>
    where
        W: Worktree,
    {
        info!("Starting {} for rev {}...", self.test.name, self.rev);

        worktree.checkout(&self.rev).await?;

        let mut cmd = self.test.command();
        let child = cmd
            .current_dir(worktree.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("LCI_COMMIT", self.rev.to_string())
            .spawn()
            .context("spawning test command")?;
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
            Either::Left((result, _)) =>
            // Test completed, figure out the result. I think maybe a true Rustacean would
            // write this block as a single chain of methods? But it seems ridiculous to me.
            {
                Ok(TestOutcome::Completed {
                    exit_code: result.map_err(anyhow::Error::from)?.code_not_killed()?,
                })
            }
            Either::Right((_, child_fut)) => {
                // Canceled. Shut down the process.
                kill(pid, Signal::SIGINT).context("couldn't interrupt child job")?;
                // We don't care about its result but we
                // need to wait for it to shut down so that we can safely give back the
                // worktree.
                let _ = child_fut.await;

                Ok(TestOutcome::Canceled)
            }
        }
    }
}

#[derive(Debug)]
pub struct TestResult {
    pub hash: CommitHash,
    // TODO: store more info here.
    test_name: String,
    result: anyhow::Result<TestOutcome>,
}

impl Display for TestResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Result for {:?}: {} => ",
            self.test_name, self.hash
        )?;
        match &self.result {
            Ok(outcome) => write!(f, "{}", outcome),
            Err(error) => write!(f, "error running test: {}", error),
        }
    }
}

// There are three results for tests: error (something went wrong when we were trying to run it),
// cancellation, and completion. Ideally we woud just have an enum with three variants, but it's
// really handy for the "error" case to be represented by std::result::Result so that we can use the
// quesiton mark operator. Thus, we have a two-layered result type... Worth it? I dunno...
#[derive(Debug, PartialEq, Eq)]
pub enum TestOutcome {
    Canceled,
    Completed { exit_code: i32 },
}

impl Display for TestOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Canceled => write!(f, "Cancelled"),
            Self::Completed { exit_code } => write!(f, "Completed - exit code {}", exit_code),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, thread::panicking, time::Duration};

    use anyhow::bail;
    use future::select_all;
    use log::error;
    use tempfile::TempDir;
    use test_case::test_case;
    use test_log;
    use tokio::{
        select,
        time::{sleep, sleep_until, Instant},
    };

    use crate::{
        git::{CommitHash, Worktree},
        test_utils::{path_exists, timeout_1s},
    };

    use super::*;

    // TODO: this sucks, find a way to dedupe more.
    struct Fixture {
        _temp_dir: TempDir,
        repo: Arc<PersistentWorktree>,
    }

    impl Fixture {
        async fn new() -> Self {
            let temp_dir = TempDir::with_prefix("fixture-").expect("couldn't make tempdir");
            let repo = PersistentWorktree::create(temp_dir.path().into())
                .await
                .expect("couldn't init test repo");
            Self {
                _temp_dir: temp_dir,
                repo: Arc::new(repo),
            }
        }
    }

    // A script that can be used as the test command for a Manager, with utilities for testing the
    // manager. The script won't terminate until told to.
    struct TestScript {
        dir: TempDir,
        script: OsString, // Raw content.
    }

    impl TestScript {
        const STARTED_FILENAME_PREFIX: &'static str = "started.";
        const SIGINTED_FILENAME_PREFIX: &'static str = "siginted.";
        const LOCK_FILENAME: &'static str = "lockfile";
        const BUG_DETECTED_PATH: &'static str = "bug_detected";

        // If this appears in the commit message , the test script will block until SIGINTed,
        // otherwise it terminates immediately.
        pub const BLOCK_COMMIT_MSG_TAG: &'static str = "block_this_test";

        // Generate a tag which, when put in the commit message of a commit, will result in the test
        // returning the given exit code.
        pub fn exit_code_tag(code: u32) -> OsString {
            return format!("exit_code({})", code).into();
        }

        // Creates a script, this will create a temporary directory, which will
        // be destroyed on drop.
        pub fn new() -> Self {
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
                "trap \"touch {siginted_path_prefix:?}$(git rev-parse HEAD); exit\" SIGINT
                touch {started_path_prefix:?}$(git rev-parse HEAD)

                if [ -e ./{lock_filename:?} ]; then
                    echo 'Overlapping test script runs used the same worktree' >> {bug_detected_path:?}
                fi
                touch ./{lock_filename:?}
                trap \"rm {lock_filename:?}\" EXIT
                commit_msg=\"$(git log -n1 --format=%B)\"
                if [[ \"$commit_msg\" =~ {block_tag} ]]; then
                    read
                fi
                # Extract the exit code and pass it to exit if there is one, otherwise pass 0.
                exit_code=$(echo \"$commit_msg\" | perl -n -e'/exit_code\\((\\d+)\\)/ && print $1')
                exit ${{exit_code:-0}}
                ",
                started_path_prefix = dir.path().join(Self::STARTED_FILENAME_PREFIX),
                siginted_path_prefix = dir.path().join(Self::SIGINTED_FILENAME_PREFIX),
                lock_filename = Self::LOCK_FILENAME,
                bug_detected_path = dir.path().join(Self::BUG_DETECTED_PATH),
                block_tag = Self::BLOCK_COMMIT_MSG_TAG,
            );

            Self {
                dir,
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
            filename.push(hash.as_ref());
            self.dir.path().join(filename)
        }

        // If this path exists, two instances of the script used the same worktree at once.
        fn bug_detected_path(&self) -> PathBuf {
            self.dir.path().join(Self::BUG_DETECTED_PATH)
        }

        // Blocks until the script is started for the given commit hash.
        pub async fn started(&self, hash: &CommitHash) -> StartedTestScript {
            path_exists(self.signalling_path(Self::STARTED_FILENAME_PREFIX, hash)).await;
            StartedTestScript {
                script: &self,
                hash: hash.to_owned(),
            }
        }

        pub fn as_test(&self) -> Test {
            Test {
                name: "my_test".to_owned(),
                program: self.program(),
                args: self.args(),
                needs_resource_idxs: vec![],
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
    }

    // anyhow::Error doesn't implement PartialEq. Here's an awkward comparator for
    // CommitTestResults, hopefully good enough for testing...?
    impl PartialEq for TestResult {
        fn eq(&self, other: &Self) -> bool {
            return self.hash == other.hash
                && match (&self.result, &other.result) {
                    (Ok(my_outcome), Ok(other_outcome)) => my_outcome == other_outcome,
                    (Err(my_err), Err(other_err)) => my_err.to_string() == other_err.to_string(),
                    _ => false,
                };
        }
    }

    impl Eq for TestResult {}

    async fn expect_results_5s(
        results: &mut broadcast::Receiver<Arc<TestResult>>,
        mut want: HashMap<CommitHash, TestOutcome>,
    ) -> anyhow::Result<()> {
        let timeout = Instant::now() + Duration::from_secs(5);
        while want.len() != 0 {
            let ctr = select!(
                _ = sleep_until(timeout) => bail!("timeout after 5s, {} results remaining", want.len()),
                output = results.recv() => output.context("test result stream terminated")?
            );
            let want_outcome = want
                .get(&ctr.hash)
                .context(format!("got result for unexpected hash {}", ctr.hash))?;
            let got_outcome = ctr
                .result
                // Some weirdness: we get Arcs with Results in them, we cannot just ? them because
                // anyhow::Error isn't Copy, it also doesn't implement Clone or anything. So, we get
                // a reference to the error and create a new error from its string representation.
                .as_ref()
                .map_err(|e| anyhow!("error testing {}: {:?}", ctr.hash, e))?;
            if *got_outcome != *want_outcome {
                bail!(
                    "unexpected test result for {}, got {} want {}",
                    ctr.hash,
                    got_outcome,
                    want_outcome
                );
            }
            want.remove(&ctr.hash);
        }
        Ok(())
    }

    async fn expect_no_more_results(
        results: &mut broadcast::Receiver<Arc<TestResult>>,
        m: &Manager,
    ) -> anyhow::Result<()> {
        select!(
            _ = sleep(Duration::from_secs(1)) => bail!("didn't settle after 1s"),
            result = results.recv() => bail!("unexpected test result received: {:?}", result),
            _ = m.settled() => Ok(())
        )
    }

    #[test_log::test(tokio::test)]
    async fn should_run_single() {
        let fixture = Fixture::new().await;
        let hash = fixture
            .repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        let script = TestScript::new();
        let mut m = Manager::builder(fixture.repo.clone(), [script.as_test()], [])
            .num_worktrees(2)
            .build()
            .await
            .expect("couldn't set up manager");
        let mut results = m.results();
        m.set_revisions(vec![hash.clone()]);
        // We should get a singular result because we only fed in one revision.
        expect_results_5s(
            &mut results,
            HashMap::from([(hash, TestOutcome::Completed { exit_code: 0 })]),
        )
        .await
        .expect("bad test result");
        expect_no_more_results(&mut results, &m).await.unwrap()
    }

    #[test_log::test(tokio::test)]
    async fn should_cancel_running() {
        let fixture = Fixture::new().await;
        // First commit's test will block forever.
        let hash1 = fixture
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG)
            .await
            .expect("couldn't create test commit");
        let script = TestScript::new();
        let mut m = Manager::builder(fixture.repo.clone(), [script.as_test()], [])
            .num_worktrees(2)
            .build()
            .await
            .expect("couldn't set up manager");
        let mut results = m.results();
        m.set_revisions(vec![hash1.clone()]);
        let started_hash1 = timeout_1s(script.started(&hash1))
            .await
            .expect("script did not run for hash1");
        // Second commit's test will terminate quickly.
        let hash2 = fixture
            .repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        m.set_revisions(vec![hash2.clone()]);
        timeout_1s(script.started(&hash2))
            .await
            .expect("script did not run for hash2");
        timeout_1s(started_hash1.siginted())
            .await
            .expect("hash1 test did not get siginted");
        expect_results_5s(
            &mut results,
            HashMap::from([
                (hash1, TestOutcome::Canceled),
                // This isn't what we're testing here but we need to assert that it comes in so we can
                // check below that nothing else comes in.
                (hash2, TestOutcome::Completed { exit_code: 0 }),
            ]),
        )
        .await
        .unwrap();
        expect_no_more_results(&mut results, &m).await.unwrap()
    }

    // This is not actually testing functionality, this is a meta-test, yikes this is
    // over-engineered.
    #[test_log::test(tokio::test)]
    async fn should_not_settle() {
        let fixture = Fixture::new().await;
        // First commit's test will block forever.
        let hash = fixture
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG)
            .await
            .expect("couldn't create test commit");
        let script = TestScript::new();
        let mut m = Manager::builder(fixture.repo.clone(), [script.as_test()], [])
            .build()
            .await
            .expect("couldn't set up manager");
        m.set_revisions([hash]);
        select!(
            _ = sleep(Duration::from_secs(1)) => (),
            _ = m.settled() => panic!("manager settled unexpectedly"),
        )
    }

    #[test_case(1, 1 ; "single worktree, one test")]
    #[test_case(4, 1 ; "multiple worktrees, one test")]
    #[test_case(4, 4 ; "multiple worktrees, multiple tests")]
    #[test_log::test(tokio::test)]
    async fn should_handle_many(num_worktrees: usize, num_tests: usize) {
        let fixture = Fixture::new().await;
        let mut hashes = Vec::new();
        let mut want_results = HashMap::new();
        let mut i = 0;
        for _ in 0..50 {
            for _ in 0..num_tests {
                let hash = fixture
                    .repo
                    // We'll give each test a unique exit code so we can check they really got
                    // tested individually.
                    .commit(TestScript::exit_code_tag(i as u32))
                    .await
                    .expect("couldn't create test commit");
                want_results.insert(hash.clone(), TestOutcome::Completed { exit_code: i });
                hashes.push(hash);
                i += 1;
            }
        }
        let script = TestScript::new();
        let mut m = Manager::builder(fixture.repo.clone(), [script.as_test()], [])
            .num_worktrees(num_worktrees)
            .build()
            .await
            .expect("couldn't set up manager");
        let mut results = m.results();
        m.set_revisions(hashes.clone());
        expect_results_5s(&mut results, want_results)
            .await
            .expect("bad reuslts");
    }

    #[test_log::test(tokio::test)]
    async fn should_respect_resource_limits() {
        let fixture = Fixture::new().await;
        let mut hashes = Vec::new();
        for _ in 0..10 {
            hashes.push(
                fixture
                    .repo
                    .commit(TestScript::BLOCK_COMMIT_MSG_TAG)
                    .await
                    .expect("couldn't create test commit"),
            );
        }
        let script = TestScript::new();
        // We only have 2 tokens
        let resource_token_counts = [2];
        // And a test that requires one of those tokens.
        let tests = [Test {
            name: "my_test".to_owned(),
            program: script.program(),
            args: script.args(),
            needs_resource_idxs: vec![1],
        }];
        let mut m = Manager::builder(fixture.repo.clone(), tests, resource_token_counts)
            .num_worktrees(4)
            .build()
            .await
            .expect("couldn't set up manager");
        m.set_revisions(hashes.clone());

        let mut start_futs = hashes.iter().map(|h| Box::pin(script.started(h))).collect();
        for _ in 0..2 {
            let (_started, _index, remaining) = timeout_1s(select_all(start_futs))
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
    // TODO: if the tests fail, the TempWorktree cleanup goes haywire, something
    // to do with panic and drop order I think.
}
