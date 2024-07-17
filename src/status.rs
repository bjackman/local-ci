use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use log::warn;

use crate::git::CommitHash;
use crate::test::TestResult;

pub struct Tracker {
    multi_progress: indicatif::MultiProgress,
    results: HashMap<CommitHash, CommitState>,
}

struct CommitState {
    spinner: indicatif::ProgressBar,
    result: Option<Arc<TestResult>>,
}

impl CommitState {
    fn pending(rev: &CommitHash) -> Self {
        let spinner = indicatif::ProgressBar::new_spinner();
        spinner.set_style(
            indicatif::ProgressStyle::with_template("[{elapsed_precise}] {spinner} {msg}")
                .expect("couldn't construct progress bar template"),
        );
        spinner.set_message(format!("pending - {}", rev));
        spinner.enable_steady_tick(Duration::from_millis(100));
        Self {
            spinner,
            result: None,
        }
    }
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            multi_progress: indicatif::MultiProgress::new(),
            results: HashMap::new(),
        }
    }

    pub fn set_revisions<T: IntoIterator<Item = CommitHash>>(&mut self, revs: T) {
        let rev_set: HashSet<CommitHash> = revs.into_iter().collect();
        self.results.retain(|k, _v| rev_set.contains(k));
        for rev in rev_set {
            if self.results.contains_key(&rev) {
                continue;
            }
            let state = CommitState::pending(&rev);
            // ProgressBar is documented as being an Arc.
            self.multi_progress.add(state.spinner.clone());
            self.results.insert(rev.to_owned(), state);
        }
    }

    pub fn update(&mut self, result: Arc<TestResult>) {
        match self.results.get_mut(&result.hash) {
            None => warn!("Unexpected result - {}", result),
            Some(state) => {
                if let Some(old_result) = state.result.replace(result.clone()) {
                    warn!("Duplicated result - {} vs {}", old_result, result);
                }
                state.spinner.finish_with_message(result.to_string());
            }
        }
    }
}
