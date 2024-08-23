use std::{
    ffi::OsStr,
    io::{stdout, Write as _},
    mem,
    sync::Arc,
};

use anyhow::{self, bail, Context as _};
use lazy_static::lazy_static;
use regex::Regex;

use crate::git::{CommitHash, Worktree};

pub struct Tracker<W: Worktree> {
    repo: Arc<W>,
}

// This ought to be private to Tracker::reset, rust just doesn't seem to let you do that.
lazy_static! {
    static ref COMMIT_HASH_REGEX: Regex = Regex::new("[0-9a-z]+").unwrap();
}

impl<W: Worktree> Tracker<W> {
    pub fn new(repo: Arc<W>) -> Self {
        Self { repo }
    }

    pub async fn reset(&self, range_spec: &OsStr, _revs: &Vec<CommitHash>) -> anyhow::Result<()> {
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
        let log_format = "%h %d %s";

        let graph_buf = self
            .repo
            // TODO: Get rid of explicit OsStr::new everywhere.
            .log_graph(range_spec, OsStr::new("%H"))
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
            cur_chunk = cur_chunk + line + "\n";
        }
        chunks.push(cur_chunk);

        let mut output = Vec::<u8>::new();
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
            let hash = CommitHash(matches.first().unwrap().as_str().to_owned());

            let log_n1_os = self
                .repo
                .log_n1(OsStr::new(&hash), OsStr::new(&log_format))
                .await
                .context(format!("couldn't get commit data for {:?}", hash))?;
            // Hack: because OsStr doesn't have a proper API, luckily we can
            // just squash to utf-8, sorry users.
            let log_n1 = log_n1_os.to_string_lossy();

            let graph_lines: Vec<&str> = chunk.split("\n").collect();
            let mut info_lines: Vec<&str> = log_n1.split("\n").collect();
            let graph_line_deficit = info_lines.len() as isize - graph_lines.len() as isize;
            if graph_line_deficit > 0 {
                panic!(
                    "TODO: vertically stretch graph ({:?} vs {:?})",
                    graph_lines, info_lines
                )
            } else {
                // Append empty entries to the info lines so that the zip below works nicely.
                info_lines.append(&mut vec![""; -graph_line_deficit as usize]);
            }
            output.append(
                &mut graph_lines
                    .iter()
                    .zip(info_lines.iter())
                    .map(|(graph, info)| (*graph).to_owned() + *info)
                    // TODO: can we get rid of the collect and just call .join on the map iterator?
                    .collect::<Vec<_>>()
                    .join("\n")
                    .into_bytes(),
            );
        }
        stdout().write(&output).context("couldn't write stdout")?;
        println!("next");
        Ok(())
    }
}
