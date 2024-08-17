use std::{collections::HashMap, sync::Arc, time::Duration};

use crate::test::{Notification, TestCase, TestStatus};

pub struct Tracker {
    multi_progress: indicatif::MultiProgress,
    trackers: HashMap<TestCase, TestCaseTracker>,
}

struct TestCaseTracker {
    test_case: TestCase,
    spinner: indicatif::ProgressBar,
}

impl TestCaseTracker {
    fn new(test_case: TestCase) -> Self {
        let spinner = indicatif::ProgressBar::new_spinner();
        spinner.set_style(
            indicatif::ProgressStyle::with_template("[{elapsed_precise}] {spinner} {msg}")
                .expect("couldn't construct progress bar template"),
        );
        spinner.enable_steady_tick(Duration::from_millis(100));
        Self {
            test_case,
            spinner,
        }
    }

    fn update(&self, status: TestStatus) {
        self.spinner
            .set_message(format!("{:?} -> {:?}", self.test_case, status));
        if status.is_final() {
           self.spinner.finish();
        }
    }
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            multi_progress: indicatif::MultiProgress::new(),
            trackers: HashMap::new(),
        }
    }

    pub fn update(&mut self, notif: Arc<Notification>) {
        self.trackers
            .entry(notif.test_case.clone())
            .or_insert_with(|| {
                // If this is violated either the notifications are coming out
                // of order, or we messed up our trackers map.
                assert!(
                    notif.status == TestStatus::initial(),
                    "unexpected initial status for test"
                );
                let tracker = TestCaseTracker::new(notif.test_case.clone());
                self.multi_progress.add(tracker.spinner.clone());
                tracker
            }).update(notif.status.clone());
    }
}
