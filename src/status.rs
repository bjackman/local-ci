use std::{
    ffi::OsStr,
    io::{stdout, Write as _},
    sync::Arc,
};

use anyhow::{self, Context as _};
use lazy_static::lazy_static;
use regex::bytes::Regex;

use crate::git::{CommitHash, Worktree};

pub struct Tracker<W: Worktree> {
    repo: Arc<W>,
}

// This ought to be private to Tracker::reset, rust just doesn't seem to let you do that.
lazy_static! {
    static ref GRAPH_ANCHOR_REGEX: Regex =
        Regex::new("(?m)^(?<graph1>.+)\t(?<hash>[0-9a-z]+) (?<posthash>.*)\n(?<graph2>.+)\n").unwrap();
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
        // I'm gonna try implementing the patching logic with just raw terminal escape codes.
        // This should be fun to try, but it might turn out this is a dumb thing
        // to try and do without some sort of cleverer framework, let's see.

        // Ideally we'd allow the user to configure arbitrary display formats
        // here, and the use null bytes to detect where we get to inject our
        // extra info.
        // This would need a bit of research to figure out if those null bytes
        // can appear autochthonously in any Git fields, I suspect that they
        // cannot but perhaps they can appear in commit messages?
        //
        // Anyway, it turns out that having null bytes in the format string
        // breaks (switches off) Git's graph generation. So for now we will just
        // have to hard-code a format string and instead use whitespace
        // as the anchor characters.
        //
        // For convenience of lookup in our own data structures we'll use the
        // full commit hash, but then for readability we'll strip that out and
        // just display Git's abbreviation of the hash (which IIUC has some
        // smarts to abbreviate as much as is safe for the given commit/repo but
        // not more).
        let fmt = "\t%H %d %h %s\n";
        let log_buf = self.repo.log_graph(range_spec, OsStr::new(&fmt)).await?;

        let mut output = Vec::new();
        for capture in GRAPH_ANCHOR_REGEX.captures_iter(log_buf.as_encoded_bytes())
        {
            capture.expand("${graph1}posthash: ${posthash}\n${graph2} COKEY\n".as_bytes(), &mut output);
        }
        stdout().write(&output).context("couldn't write stdout")?;
        println!("next");
        Ok(())
    }
}
