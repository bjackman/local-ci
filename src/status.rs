use std::{
    collections::HashMap,
    ffi::OsStr,
    io::{stdout, Write},
    mem,
    process::Output,
    sync::Arc,
};

use anyhow::{self, bail, Context as _};
use lazy_static::lazy_static;
use regex::Regex;

use crate::{
    git::{CommitHash, Worktree},
    test::{Notification, TestStatus},
};

pub struct Tracker<W: Worktree, O: Write> {
    repo: Arc<W>,
    // Inner string key is test name.
    statuses: HashMap<CommitHash, HashMap<String, TestStatus>>,
    output_buf: OutputBuffer,
    output: O,
}

// This ought to be private to Tracker::reset, rust just doesn't seem to let you do that.
lazy_static! {
    static ref COMMIT_HASH_REGEX: Regex = Regex::new("[0-9a-z]{40,}").unwrap();
}

impl<W: Worktree, O: Write> Tracker<W, O> {
    pub fn new(repo: Arc<W>, output: O) -> Self {
        Self {
            repo,
            statuses: HashMap::new(),
            output_buf: OutputBuffer::empty(),
            output,
        }
    }

    pub async fn set_range(
        &mut self,
        range_spec: &OsStr,
        revs: &Vec<CommitHash>,
    ) -> anyhow::Result<()> {
        self.output_buf = OutputBuffer::new(&self.repo, range_spec, revs).await?;
        Ok(())
    }

    pub fn update(&mut self, notif: Arc<Notification>) {
        let commit_statuses = self
            .statuses
            .entry(notif.test_case.hash.clone())
            .or_insert(HashMap::new());
        commit_statuses.insert(notif.test_case.test_name.clone(), notif.status.clone());
    }

    pub fn repaint(&mut self) -> anyhow::Result<()> {
        self.output_buf.render(&mut self.output, &self.statuses)
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
    // TODO use Default instead?
    pub fn empty() -> Self {
        Self {
            lines: Vec::new(),
            status_commits: HashMap::new(),
        }
    }

    pub async fn new<W: Worktree>(
        repo: &Arc<W>,
        range_spec: &OsStr,
        _revs: &Vec<CommitHash>,
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

        // This should eventually be configurable.
        let log_format =
            "%Cred%h%Creset -%C(yellow)%d%Creset %s %Cgreen(%cr) %C(bold blue)<%an>%Creset";

        let graph_buf = repo
            // TODO: Get rid of explicit OsStr::new everywhere.
            .log_graph(range_spec, OsStr::new("%H\n"))
            .await?
            // OsStr doesn't have a proper API, luckily we can expect utf-8.
            .into_string()
            .map_err(|_err| anyhow::anyhow!("got non-utf8 output from git log"))?;

        // TODO: do this without all the copying!
        let mut cur_chunk = String::new();
        let mut chunks = Vec::new();
        for line in graph_buf.split("\n") {
            // --graph uses * to represent a node in the DAG.
            if line.contains("*") && !cur_chunk.is_empty() {
                chunks.push(mem::take(&mut cur_chunk));
            }
            cur_chunk = cur_chunk + line;
        }
        chunks.push(cur_chunk);

        let mut lines = Vec::new();
        let mut status_commits = HashMap::new();
        for chunk in chunks {
            // The commit hash should be the only alphanumeric sequence in
            // the chunk.
            let matches: Vec<_> = COMMIT_HASH_REGEX.find_iter(&chunk).collect();
            if matches.len() != 1 {
                bail!(
                    "matched {} commit hashes in graph chunk:\n{:?}",
                    matches.len(),
                    chunk
                );
            }
            let mattch = matches.first().unwrap();
            let hash = CommitHash(mattch.as_str().to_owned());

            let log_n1_os = repo
                .log_n1(OsStr::new(&hash), OsStr::new(&log_format))
                .await
                .context(format!("couldn't get commit data for {:?}", hash))?;
            // Hack: because OsStr doesn't have a proper API, luckily we can
            // just squash to utf-8, sorry users.
            let log_n1 = log_n1_os.to_string_lossy();

            // We're gonna add our own newlines in so we don't need the one that
            // Git printed.
            let log_n1 = log_n1.strip_suffix('\n').unwrap_or(&log_n1);

            let mut graph_lines: Vec<&str> = chunk.split("\n").collect();

            // We only want the graph bit, strip out the commit hash which we
            // only put in there as an anchor for this algorithm.
            graph_lines[0] = &graph_lines[0][..mattch.range().start];

            let extension_line;
            let mut info_lines: Vec<&str> = log_n1.split("\n").collect();

            // Here's where we'll inject the live status
            status_commits.insert(info_lines.len(), hash);
            info_lines.push("");

            let graph_line_deficit = info_lines.len() as isize - graph_lines.len() as isize;
            if graph_line_deficit > 0 {
                // We assume that the first line of the chunk will contain an
                // asterisk identifying the current commit, and some vertical
                // lines continuing up to the previous chunk. We just copy those
                // vertical lines and then add a new vertical lines pointing up
                // to the asterisk.
                //
                // TODO: Is there any situation where Git actually uses diagonal
                // lines here on the same line as the *? Ideally I should read
                // the Git code but I CBA. I could at least graph the whole
                // Linux kernel history and see if it ever arises there.
                extension_line = graph_lines[0].replace("*", "|");
                for _ in 0..graph_line_deficit {
                    graph_lines.insert(1, &extension_line);
                }
            } else {
                // Append empty entries to the info lines so that the zip below works nicely.
                info_lines.append(&mut vec![""; -graph_line_deficit as usize]);
            }
            assert_eq!(info_lines.len(), graph_lines.len());

            lines.append(
                &mut graph_lines
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

    // TODO: Use AsyncWrite.
    fn render(
        &self,
        output: &mut impl Write,
        statuses: &HashMap<CommitHash, HashMap<String, TestStatus>>,
    ) -> anyhow::Result<()> {
        for (i, line) in self.lines.iter().enumerate() {
            output.write(line.as_bytes())?;
            if let Some(hash) = self.status_commits.get(&i) {
                match statuses.get(&hash) {
                    Some(statuses) => {
                        let mut statuses: Vec<(&String, &TestStatus)> = statuses.iter().collect();
                        // Sort by test case name. Would like sort_by_key here but
                        // there's lifetime pain.
                        statuses.sort_by(|(name1, _), (name2, _)| name1.cmp(name2));
                        for (name, status) in statuses {
                            output.write(format!("{name}: {status} ").as_bytes())?;
                        }
                    }
                    None => {
                        output.write(format!("UNKNOWN").as_bytes())?;
                    }
                }
            }
            output.write(&[b'\n'])?;
        }
        Ok(())
    }
}
