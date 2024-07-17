use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use log::warn;

use crate::git::CommitHash;
use crate::test::TestResult;

pub struct Tracker {
    // If there is no result, it means that test is pending.
    results: HashMap<CommitHash, Option<Arc<TestResult>>>,
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            results: HashMap::new(),
        }
    }

    pub fn set_revisions<T: IntoIterator<Item = CommitHash>>(&mut self, revs: T) {
        let rev_set: HashSet<CommitHash> = revs.into_iter().collect();
        self.results.retain(|k, _v| rev_set.contains(k));
        for rev in rev_set {
            self.results.insert(rev.to_owned(), None);
        }
    }

    pub fn update(&mut self, result: Arc<TestResult>) {
        match self.results.insert(result.hash.clone(), Some(result.clone())) {
            Some(None) => (), // Expected
            Some(Some(old_result)) => warn!("Duplicated result - {} vs {}", old_result, result),
            None => warn!("Unexpected result - {}", result),
        }
    }
}
