use std::sync::Arc;

use crate::git::CommitHash;
use crate::test::Notification;

pub struct Tracker {
    // multi_progress: indicatif::MultiProgress,
    // results: HashMap<CommitHash, CommitState>,
}

// struct CommitState {
//     // spinner: indicatif::ProgressBar,
//     // result: Option<Arc<TestResult>>,
// }

// impl CommitState {
//     fn pending(rev: &CommitHash) -> Self {
//         let spinner = indicatif::ProgressBar::new_spinner();
//         spinner.set_style(
//             indicatif::ProgressStyle::with_template("[{elapsed_precise}] {spinner} {msg}")
//                 .expect("couldn't construct progress bar template"),
//         );
//         spinner.set_message(format!("pending - {}", rev));
//         spinner.enable_steady_tick(Duration::from_millis(100));
//         Self {
//             // spinner,
//         }
//     }
// }

impl Tracker {
    pub fn new() -> Self {
        Self {
            // multi_progress: indicatif::MultiProgress::new(),
            // results: HashMap::new(),
        }
    }

    pub fn set_revisions<T: IntoIterator<Item = CommitHash>>(&mut self, _revs: T) {}

    pub fn update(&mut self, _result: Arc<Notification>) {
        // match self.results.get_mut(&result.test_case.hash) {
        //     None => warn!("Unexpected result - {}", result),
        //     Some(state) => {
        //         if let Some(old_result) = state.result.replace(result.clone()) {
        //             warn!("Duplicated result - {} vs {}", old_result, result);
        //         }
        //         state.spinner.finish_with_message(result.to_string());
        //     }
        // }
    }
}
