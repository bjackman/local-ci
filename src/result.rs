use std::{
    fs::{self, create_dir_all, remove_dir_all, File},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::{
    git::Hash,
    test::{ConfigHash, TestCase, TestName, TestResult},
};

// Result database similar to the design described in
// https://github.com/bjackman/git-brisect?tab=readme-ov-file#the-result-directory
// TODO: Actually we should probably separate it by the repo lol. But how?
pub struct Database {
    base_dir: PathBuf,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
struct TestResultEntry {
    config_hash: ConfigHash,
    result: TestResult,
}

impl Database {
    pub fn create_or_open(base_dir: &Path) -> anyhow::Result<Self> {
        create_dir_all(base_dir).context(format!(
            "creating result database dir at {}",
            base_dir.display()
        ))?;
        Ok(Self {
            base_dir: base_dir.to_owned(),
        })
    }

    pub fn result_relpath(test_case: &TestCase) -> PathBuf {
        Path::new(test_case.storage_hash()).join(&test_case.test.name)
    }

    fn result_path(&self, hash: &Hash, test_name: &TestName) -> PathBuf {
        self.base_dir.join(hash.as_ref()).join(test_name)
    }

    pub fn cached_result(
        &self,
        hash: &Hash,
        test_name: &TestName,
        // Hash of the config that created the test.
        config_hash: ConfigHash,
    ) -> Result<Option<TestResult>> {
        let result_path = self.result_path(hash, test_name).join("result.json");
        if !result_path.exists() {
            return Ok(None);
        }

        let entry: TestResultEntry =
            serde_json::from_str(&fs::read_to_string(result_path).context("reading result JSON")?)
                .context("parsing result JSON")?;
        if entry.config_hash != config_hash {
            // Configuration changed, need to re-run.
            return Ok(None);
        }

        Ok(Some(entry.result))
    }

    // Prepare to create the output directory for a job output, but don't actually create it yet.
    // It's created once you use one of the methods of CommitOutput for writing data.
    pub fn create_output(&self, test_case: &TestCase) -> anyhow::Result<TestCaseOutput> {
        TestCaseOutput::new(
            self.result_path(test_case.storage_hash(), &test_case.test.name),
            test_case.test.config_hash,
        )
    }
}

// Output for an individual commit.
pub struct TestCaseOutput {
    base_dir: PathBuf,
    base_dir_created: bool,
    stdout_opened: bool,
    stderr_opened: bool,
    status_written: bool,
    config_hash: ConfigHash,
}

impl TestCaseOutput {
    pub fn new(base_dir: PathBuf, config_hash: ConfigHash) -> anyhow::Result<Self> {
        Ok(Self {
            base_dir,
            base_dir_created: false,
            stdout_opened: false,
            stderr_opened: false,
            status_written: false,
            config_hash,
        })
    }

    // Create and return base directory
    fn get_base_dir(&mut self) -> Result<&Path> {
        if !self.base_dir_created {
            // To avoid confusion from partially-overwritten entries, delete the old one if it exists.
            if self.base_dir.exists() {
                remove_dir_all(&self.base_dir).context("cleaning up old result DB entry")?;
            }

            create_dir_all(&self.base_dir).context(format!(
                "creating commit result dir at {}",
                self.base_dir.display()
            ))?;
            self.base_dir_created = true;
        }
        Ok(&self.base_dir)
    }

    // Panics if called more than once.
    pub fn stdout(&mut self) -> Result<File> {
        assert!(!self.stdout_opened);
        self.stdout_opened = true;
        Ok(File::create(self.get_base_dir()?.join("stdout.txt"))?)
    }

    // Panics if called more than once.
    pub fn stderr(&mut self) -> Result<File> {
        assert!(!self.stderr_opened);
        self.stderr_opened = true;
        Ok(File::create(self.get_base_dir()?.join("stderr.txt"))?)
    }

    // TODO: Figure out how to record errors in the more general case, probably with a JSON object.
    // Panics if called more than once.
    pub fn set_result(&mut self, result: &TestResult) -> anyhow::Result<()> {
        assert!(!self.status_written);
        self.status_written = true;
        let entry = TestResultEntry {
            config_hash: self.config_hash,
            result: result.clone(),
        };
        Ok(fs::write(
            self.get_base_dir()?.join("result.json"),
            serde_json::to_vec(&entry).expect("failed to serialize TestStatus"),
        )?)
    }
}

// TODO:
// - Test behaviour on already-existing directories
