use std::{collections::HashMap, sync::Arc, time::Duration};

use log::error;

use crate::test::{Notification, TestCase, TestStatus};

pub struct Tracker {
    multi_progress: indicatif::MultiProgress,
    trackers: HashMap<TestCase, TestCaseTracker>,
}

struct TestCaseTracker {
    test_case: TestCase,
    status: TestStatus,
    spinner: indicatif::ProgressBar,
}

impl TestCaseTracker {
    // fn new(test_case) -> Self {
    //     let spinner = indicatif::ProgressBar::new_spinner();
    //     spinner.set_style(
    //         indicatif::ProgressStyle::with_template("[{elapsed_precise}] {spinner} {msg}")
    //             .expect("couldn't construct progress bar template"),
    //     );
    //     spinner.set_message(format!("pending - {}", rev));
    //     spinner.enable_steady_tick(Duration::from_millis(100));
    //     Self {
    //         // spinner,
    //     }
    // }
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            multi_progress: indicatif::MultiProgress::new(),
            trackers: HashMap::new(),
        }
    }

    pub fn update(&mut self, notif: Arc<Notification>) {
        let tracker = self
            .trackers
            .entry(notif.test_case.clone())
            .or_insert_with(|| {
                let spinner = indicatif::ProgressBar::new_spinner();
                spinner.set_style(
                    indicatif::ProgressStyle::with_template("[{elapsed_precise}] {spinner} {msg}")
                        .expect("couldn't construct progress bar template"),
                );
                spinner.enable_steady_tick(Duration::from_millis(100));
                self.multi_progress.add(spinner.clone());
                TestCaseTracker {
                    test_case: notif.test_case.clone(),
                    status: notif.status.clone(),
                    spinner,
                }
            });
        tracker
            .spinner
            .set_message(format!("{:?} -> {:?}", notif.test_case, notif.status));
        if notif.status.is_final() {
            tracker.spinner.finish();
        }
    }
}
