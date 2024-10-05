use std::{collections::HashMap, ffi::OsStr, io::Write, mem, sync::Arc};

use ansi_control_codes::control_sequences::{CPL, ED};
use anyhow::{self, bail, Context as _};
use colored::Colorize;
use lazy_static::lazy_static;
use regex::Regex;

use crate::{
    git::{CommitHash, Worktree},
    result::Database,
    test::{Notification, TestCase, TestName, TestStatus},
};

struct TrackedTestCase {
    test_case: TestCase,
    status: TestStatus,
}

// Inner string key is test name. Here we awkwardly store this as a
// two-level map instead of a flat one by TestCaseId, because that
// conveniently lets us grab all the TestCases for a given commit when
// rendering the output.
type TrackedCases = HashMap<CommitHash, HashMap<TestName, TrackedTestCase>>;

// Updates the awkward nested hashmap to reflect a new notification coming in.
// Standalone function for convenient use in tests.
fn update_tracked_cases(tracked_cases: &mut TrackedCases, notif: Arc<Notification>) {
    let commit_statuses = tracked_cases
        .entry(notif.test_case.commit_hash.clone())
        .or_default();
    commit_statuses.insert(
        notif.test_case.test.name.clone(),
        TrackedTestCase {
            test_case: notif.test_case.clone(),
            status: notif.status.clone(),
        },
    );
}

pub struct Tracker<W: Worktree, O: Write> {
    repo: Arc<W>,
    tracked_cases: TrackedCases,
    output_buf: OutputBuffer,
    output: O,
    lines_to_clear: usize,
    result_url_base: String,
}

// This ought to be private to Tracker::reset, rust just doesn't seem to let you do that.
lazy_static! {
    static ref COMMIT_HASH_REGEX: Regex = Regex::new("[0-9a-z]{40,}").unwrap();
    static ref GRAPH_COMPONENT_REGEX: Regex = Regex::new(r"[\\/\*]").unwrap();
}

impl<W: Worktree, O: Write> Tracker<W, O> {
    pub fn new(repo: Arc<W>, output: O, result_url_base: impl Into<String>) -> Self {
        Self {
            repo,
            tracked_cases: HashMap::new(),
            output_buf: OutputBuffer::empty(),
            output,
            lines_to_clear: 0,
            result_url_base: result_url_base.into(),
        }
    }

    pub async fn set_range(&mut self, range_spec: &OsStr) -> anyhow::Result<()> {
        // This should eventually be configurable.
        let log_format =
            "%Cred%h%Creset -%C(yellow)%d%Creset %s %Cgreen(%cr) %C(bold blue)<%an>%Creset";

        self.output_buf = OutputBuffer::new(&self.repo, range_spec, log_format).await?;
        Ok(())
    }

    pub fn update(&mut self, notif: Arc<Notification>) {
        update_tracked_cases(&mut self.tracked_cases, notif);
    }

    pub fn repaint(&mut self) -> anyhow::Result<()> {
        if self.lines_to_clear != 0 {
            // CPL is "cursor previous line" i.e. move the cursor up N lines.
            // ED is "erase display", which by default means cleareverything after the cursor.
            // The library we're using here doesn't seem to provide an obvious
            // way to just get at the bytes, other than formatting it.
            write!(
                &mut self.output,
                "{}{}",
                CPL(Some(self.lines_to_clear as u32)),
                ED(None)
            )?;
        }
        self.lines_to_clear =
            self.output_buf
                .render(&mut self.output, &self.tracked_cases, &self.result_url_base)?;
        Ok(())
    }
}

// Represents the buffer showing the current status of all the commits being tested.
struct OutputBuffer {
    // Pre-rendered lines containing static information (graph, commit log info etc).
    lines: Vec<String>,
    // lines[i] should be appended with the live status information of tests for status_commit[i].
    status_commits: HashMap<usize, CommitHash>,
}

impl OutputBuffer {
    pub fn empty() -> Self {
        Self {
            lines: Vec::new(),
            status_commits: HashMap::new(),
        }
    }

    pub async fn new<W: Worktree, S: AsRef<OsStr>>(
        repo: &Arc<W>,
        range_spec: S,
        log_format: &str,
    ) -> anyhow::Result<Self> {
        // All right this is gonna seem pretty hacky. We're gonna get the --graph log
        // as a text blob, then we're gonna use our pre-existing knowledge about
        // its contents as position anchors to patch it with the information we need.
        // This saves us having to actually write any algorithms ourselves. Basically
        // we only care about the structure of the DAG in so far as it influences the layout
        // of characters we're gonna display in the terminal. So, we just get
        // Git to tell us that exact information 🤷.
        // This is actually the same approach taken by the code I looked at in
        // the edamagit VSCode extension.
        // Note it's tricky because, even if you simplify it by fixing the
        // number of lines that the non-graph section of the output occupies,
        // the graph logic can still sometimes occupy more more lines when
        // history is very complex.
        //
        // So here's the idea: we just git git to dump out the graph. We divide
        // this graph buffer into chunks that begin at the start of a line that
        // contains a commit hash. This will look something like:
        /*

         | * |   e96277a570cd32432fjklfef
         | |\ \
         | | |/
         | |/|

        */
        // We want to display a) some more human-readable information about the
        // commit (i.e. what you get from logging with a more informative
        // --format) and b) our injected test status data. Overall this will
        // produce some other buffer. If it has less lines than the graph buffer
        // chunk, we can just append those lines onto the lines of the graph
        // buffer pairwise. If it has more lines then we will need to stretch
        // out the graph vertically to make space first.

        let graph_buf = repo
            .log_graph(range_spec.as_ref(), "%H\n")
            .await?
            // OsStr doesn't have a proper API, luckily we can expect utf-8.
            .into_string()
            .map_err(|_err| anyhow::anyhow!("got non-utf8 output from git log"))?;
        let graph_buf = graph_buf.trim();

        // Each chunk is a Vec of lines.
        let mut cur_chunk = Vec::<&str>::new();
        let mut chunks = Vec::<Vec<&str>>::new();
        for line in graph_buf.split('\n') {
            // --graph uses * to represent a node in the DAG.
            if line.contains('*') && !cur_chunk.is_empty() {
                chunks.push(mem::take(&mut cur_chunk));
            }
            if !line.is_empty() {
                cur_chunk.push(line);
            }
        }
        if !cur_chunk.is_empty() {
            chunks.push(cur_chunk);
        }

        let mut lines = Vec::new();
        let mut status_commits = HashMap::new();
        for mut chunk in chunks {
            // The commit hash should be the only alphanumeric sequence in
            // the chunk and it should be in the first line.
            let matches: Vec<_> = COMMIT_HASH_REGEX.find_iter(chunk[0]).collect();
            if matches.len() != 1 {
                bail!(
                    "matched {} commit hashes in graph chunk:\n{:?}",
                    matches.len(),
                    chunk
                );
            }
            let mattch = matches.first().unwrap();
            let hash = CommitHash::new(mattch.as_str());

            let log_n1_os = repo
                .log_n1(&hash, log_format)
                .await
                .context(format!("couldn't get commit data for {:?}", hash))?;
            // Hack: because OsStr doesn't have a proper API, luckily we can
            // just squash to utf-8, sorry users.
            let log_n1 = log_n1_os.to_string_lossy();

            // We're gonna add our own newlines in so we don't need the one that
            // Git printed.
            let log_n1 = log_n1.strip_suffix('\n').unwrap_or(&log_n1);

            // We only want the graph bit, strip out the commit hash which we
            // only put in there as an anchor for this algorithm.
            chunk[0] = &chunk[0][..mattch.range().start];

            let mut info_lines: Vec<&str> = log_n1.split('\n').collect();

            // Here's where we'll inject the live status
            status_commits.insert(lines.len() + info_lines.len(), hash);
            info_lines.push("");

            let graph_line_deficit = info_lines.len() as isize - chunk.len() as isize;
            let extension_line;
            if graph_line_deficit > 0 {
                // We assume that the first line of the chunk will contain an
                // asterisk identifying the current commit, and some vertical
                // lines continuing up to the previous chunk. We just copy those
                // vertical lines and then add a new vertical lines pointing up
                // to the asterisk.
                //
                // I checked and it is in fact possible to have non-vertical
                // lines on the same line as the asterisk. E.g. check the linux
                // kernel history, search back to commit 578cc98b66f5a5 and you
                // will see it. So we need to replace diagnoals with verticals
                // too.
                extension_line = GRAPH_COMPONENT_REGEX.replace_all(chunk[0], "|");
                for _ in 0..graph_line_deficit {
                    chunk.insert(1, &extension_line);
                }
            } else {
                // Append empty entries to the info lines so that the zip below works nicely.
                info_lines.append(&mut vec![""; -graph_line_deficit as usize]);
            }
            assert_eq!(info_lines.len(), chunk.len());

            lines.append(
                &mut chunk
                    .iter()
                    .zip(info_lines.iter())
                    .map(|(graph, info)| (*graph).to_owned() + *info)
                    // TODO: can we get rid of the collect and just call .join on the map iterator?
                    .collect::<Vec<_>>(),
            );
        }
        Ok(Self {
            lines,
            status_commits,
        })
    }

    // Returns number of lines that were written.
    // TODO: Use AsyncWrite.
    fn render(
        &self,
        output: &mut impl Write,
        statuses: &HashMap<CommitHash, HashMap<TestName, TrackedTestCase>>,
        result_url_base: &str,
    ) -> anyhow::Result<usize> {
        if self.lines.is_empty() {
            output.write_all(b"[range empty]\n")?;
            return Ok(1);
        }
        for (i, line) in self.lines.iter().enumerate() {
            output.write_all(line.as_bytes())?;
            if let Some(hash) = self.status_commits.get(&i) {
                if let Some(tracked_cases) = statuses.get(hash) {
                    self.render_cases(output, tracked_cases, result_url_base)?;
                }
            }
            output.write_all(b"\n")?;
        }
        Ok(self.lines.len())
    }

    fn render_cases(
        &self,
        output: &mut impl Write,
        tracked_cases: &HashMap<TestName, TrackedTestCase>,
        result_url_base: &str,
    ) -> anyhow::Result<()> {
        let mut tracked_cases: Vec<(&TestName, &TrackedTestCase)> = tracked_cases.iter().collect();
        // Sort by test case name. Would like sort_by_key here but
        // there's lifetime pain.
        tracked_cases.sort_by(|(name1, _), (name2, _)| name1.cmp(name2));
        for (name, tracked_case) in tracked_cases {
            let status_part = match &tracked_case.status {
                TestStatus::Error(msg) => msg.on_bright_red(),
                TestStatus::Completed(result) => {
                    if result.exit_code == 0 {
                        "success".on_green()
                    } else {
                        format!("failed (status {})", result.exit_code).on_red()
                    }
                }
                _ => tracked_case.status.to_string().into(),
            }
            // IIUC the to_string renders the ColoredString. If you just implicitly convert
            // it to a str it skips the colour rendering.
            .to_string();
            let url = format!(
                "{}/{}/stdout.txt",
                result_url_base,
                Database::result_relpath(&tracked_case.test_case).to_string_lossy()
            );
            let status_part = hyperlink(&status_part, &url);
            output
                .write_all(format!("{}: {} ", name.to_string().bold(), status_part).as_bytes())?;
        }
        Ok(())
    }
}

// Renders a hyperlink like in
// https://gist.github.com/egmontkob/eb114294efbcd5adb1944c9f3cb5feda.
// Obviously it would be a bit nicer to use some library for this, and we
// already do that for the colorization, but I just dunno if it's worth
// pulling in a dependency for something so trivial.
fn hyperlink(text: &str, url: &str) -> String {
    format!("\u{1b}]8;;{}\u{1b}\\{}\u{1b}]8;;\u{1b}\\", url, text)
}

#[cfg(test)]
mod tests {
    use core::str;
    use std::{io::BufWriter, sync::Arc, time::Duration};

    use googletest::{expect_that, prelude::eq};

    use crate::{
        git::test_utils::{TempRepo, WorktreeExt},
        test::{CachePolicy, Test, TestName, TestResult},
        test_utils::some_time,
    };

    use super::*;

    fn fake_test(name: &str, cache_policy: CachePolicy) -> Arc<Test> {
        Arc::new(Test {
            name: TestName::new(name),
            cache_policy,
            // Don't care abou any of the other fields in these tests
            config_hash: 0,
            program: "".into(),
            args: vec![],
            needs_resources: [].into(),
            shutdown_grace_period: Duration::from_secs(1),
            depends_on: vec![],
        })
    }

    fn fake_notif(commit_hash: &CommitHash, test: &Arc<Test>, status: TestStatus) -> Notification {
        Notification {
            test_case: TestCase {
                commit_hash: commit_hash.clone(),
                cache_hash: Some(commit_hash.clone().into()),
                test: test.clone(),
            },
            status,
        }
    }

    #[googletest::test]
    #[test_log::test(tokio::test)]
    async fn output_buffer_smoke() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        repo.commit("1", some_time()).await.unwrap();
        let hash2 = repo.commit("2", some_time()).await.unwrap();
        let hash3 = repo.commit("3", some_time()).await.unwrap();
        let test1 = fake_test("my_test1", CachePolicy::ByCommit);
        let test2 = fake_test("my_test2", CachePolicy::ByCommit);

        let ob = OutputBuffer::new(&repo, format!("{hash2}^..HEAD"), "%h %s")
            .await
            .expect("failed to build OutputBuffer");
        let mut tracked_cases = HashMap::new();
        for notif in [
            fake_notif(&hash3, &test1, TestStatus::Enqueued),
            fake_notif(
                &hash3,
                &test2,
                TestStatus::Completed(TestResult { exit_code: 0 }),
            ),
            fake_notif(&hash2, &test1, TestStatus::Error("oh no".to_owned())),
            fake_notif(&hash2, &test2, TestStatus::Started),
        ] {
            update_tracked_cases(&mut tracked_cases, Arc::new(notif));
        }

        let mut buf = BufWriter::new(Vec::new());
        ob.render(&mut buf, &tracked_cases, "myhost")
            .expect("OutputBuffer::render failed");

        expect_that!(
            // The colored crate does not have any useful way to disable it from
            // this test code, only globally. This clashes with parallel testing.
            // At first I thought about just having tests take a global lock but
            // then realied that if one test failed, the other would panic
            // holding the lock. Also parallelism is nice. So, we just ignore
            // the color.
            *strip_ansi_escapes::strip_str(str::from_utf8(&buf.into_inner().unwrap()).unwrap()),
            eq("* 08e80af 3\n\
                | my_test1: Enqueued my_test2: success \n\
                * b29043f 2\n\
                | my_test1: oh no my_test2: Started \n")
        );
    }

    #[googletest::test]
    #[test_log::test(tokio::test)]
    async fn output_buffer_octopus() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let base_hash = repo.commit("base", some_time()).await.unwrap();
        repo.commit("join", some_time()).await.unwrap();
        let hash1 = repo.commit("1", some_time()).await.unwrap();
        repo.checkout(&base_hash).await.unwrap();
        let hash2 = repo.commit("2", some_time()).await.unwrap();
        repo.checkout(&base_hash).await.unwrap();
        let hash3 = repo.commit("3", some_time()).await.unwrap();
        repo.merge(&[hash1, hash2.clone(), hash3.clone()], some_time())
            .await
            .unwrap();
        let test1 = fake_test("my_test1", CachePolicy::ByCommit);
        let test2 = fake_test("my_test2", CachePolicy::ByCommit);

        let ob = OutputBuffer::new(&repo, format!("{base_hash}..HEAD"), "%h %s")
            .await
            .expect("failed to build OutputBuffer");

        let mut tracked_cases = HashMap::new();
        for notif in [
            fake_notif(&hash3, &test1, TestStatus::Enqueued),
            fake_notif(
                &hash3,
                &test2,
                TestStatus::Completed(TestResult { exit_code: 0 }),
            ),
            fake_notif(&hash2, &test1, TestStatus::Error("oh no".to_owned())),
            fake_notif(&hash2, &test2, TestStatus::Started),
        ] {
            update_tracked_cases(&mut tracked_cases, Arc::new(notif));
        }

        let mut buf = BufWriter::new(Vec::new());
        ob.render(&mut buf, &tracked_cases, "myhost")
            .expect("OutputBuffer::render failed");

        // Note this is a kinda weird log. We excluded the common ancestor of all the commits.
        // Also note it's a kinda weird input because we haven't provided any
        // statuses all of the commits (this does momentarily happen IRL).
        expect_that!(
            *strip_ansi_escapes::strip_str(str::from_utf8(&buf.into_inner().unwrap()).unwrap()),
            eq("*-.   05d10f7 merge commit\n\
                |\\ \\  \n\
                | | | \n\
                | | * eea5ddf 2\n\
                | |   my_test1: oh no my_test2: Started \n\
                | * 839dc2e 1\n\
                | | \n\
                | * 7de308a join\n\
                |   \n\
                * 02ad53b 3\n\
                | my_test1: Enqueued my_test2: success \n")
        );
    }

    #[googletest::test]
    #[test_log::test(tokio::test)]
    async fn output_buffer_empty() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let base_hash = repo.commit("base", some_time()).await.unwrap();
        repo.commit("join", some_time()).await.unwrap();
        let hash1 = repo.commit("1", some_time()).await.unwrap();
        repo.checkout(&base_hash).await.unwrap();
        let hash2 = repo.commit("2", some_time()).await.unwrap();
        repo.checkout(&base_hash).await.unwrap();
        let hash3 = repo.commit("3", some_time()).await.unwrap();
        repo.merge(&[hash1, hash2.clone(), hash3.clone()], some_time())
            .await
            .unwrap();
        let test1 = fake_test("my_test1", CachePolicy::ByCommit);
        let test2 = fake_test("my_test2", CachePolicy::ByCommit);

        let ob = OutputBuffer::new(&repo, format!("{base_hash}..{base_hash}"), "%h %s")
            .await
            .expect("failed to build OutputBuffer");
        let mut tracked_cases = HashMap::new();
        for notif in [
            fake_notif(&hash3, &test1, TestStatus::Enqueued),
            fake_notif(
                &hash3,
                &test1,
                TestStatus::Completed(TestResult { exit_code: 0 }),
            ),
            fake_notif(&hash2, &test2, TestStatus::Error("oh no".to_owned())),
            fake_notif(&hash2, &test2, TestStatus::Started),
        ] {
            update_tracked_cases(&mut tracked_cases, Arc::new(notif));
        }

        let mut buf = BufWriter::new(Vec::new());
        ob.render(&mut buf, &tracked_cases, "myhost")
            .expect("OutputBuffer::render failed");

        expect_that!(
            *strip_ansi_escapes::strip_str(str::from_utf8(&buf.into_inner().unwrap()).unwrap()),
            eq("[range empty]\n".to_owned())
        );
    }
}
