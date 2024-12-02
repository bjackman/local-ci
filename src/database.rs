use std::{
    fs::{self, create_dir_all, remove_dir_all, File},
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result};
#[allow(unused_imports)]
use log::debug;
use serde::{Deserialize, Serialize};

use crate::{
    git::Hash,
    test::{ConfigHash, TestCase, TestJobOutput, TestName, TestResult},
};

// Result database similar to the design described in
// https://github.com/bjackman/git-brisect?tab=readme-ov-file#the-result-directory
// TODO: Actually we should probably separate it by the repo lol. But how?
pub struct Database {
    pub base_dir: PathBuf,
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
        self.base_dir.join::<&str>(hash.as_ref()).join(test_name)
    }

    pub fn lookup_result(&self, test_case: &TestCase) -> Result<Option<DatabaseEntry>> {
        let hash = match test_case.cache_hash {
            None => return Ok(None),
            Some(ref hash) => hash,
        };

        let base_dir = self.result_path(hash, &test_case.test.name);
        if !base_dir.exists() {
            return Ok(None);
        }
        let entry = DatabaseEntry::open(&base_dir)?;

        if entry.result.config_hash != test_case.test.config_hash {
            // Configuration changed, need to re-run.
            return Ok(None);
        }

        Ok(Some(entry))
    }

    // Prepare to create the output directory for a job output, but don't actually create it yet.
    // It's created once you use one of the methods of CommitOutput for writing data.
    pub fn create_output(&self, test_case: &TestCase) -> anyhow::Result<DatabaseOutput> {
        DatabaseOutput::new(
            self.result_path(test_case.storage_hash(), &test_case.test.name),
            test_case.test.config_hash,
        )
    }
}

// Existing entry in the database.
pub struct DatabaseEntry {
    base_path: PathBuf,
    result: TestResultEntry,
}

impl DatabaseEntry {
    fn open(base_dir: &Path) -> anyhow::Result<Self> {
        let json_path = base_dir.join("result.json");
        Ok(Self {
            base_path: base_dir.to_owned(),
            result: serde_json::from_str(
                &fs::read_to_string(base_dir.join("result.json"))
                    .with_context(|| format!("reading result JSON from {:?}", json_path))?,
            )
            .context("parsing result JSON")?,
        })
    }

    pub fn result(&self) -> &TestResult {
        &self.result.result
    }

    pub fn stdout_path(&self) -> PathBuf {
        self.base_path.join("stdout.txt")
    }

    pub fn stderr_path(&self) -> PathBuf {
        self.base_path.join("stderr.txt")
    }

    pub fn artifacts_dir(&self) -> PathBuf {
        self.base_path.join("artifacts")
    }
}

// Output for an individual test job, stored into the database
pub struct DatabaseOutput {
    base_dir: PathBuf,
    artifacts_dir: PathBuf,
    base_dir_created: bool,
    stdout_opened: bool,
    stderr_opened: bool,
    status_written: bool,
    config_hash: ConfigHash,
}

impl DatabaseOutput {
    pub fn new(base_dir: PathBuf, config_hash: ConfigHash) -> anyhow::Result<Self> {
        debug!("Creating database entry at {base_dir:?}");
        Ok(Self {
            artifacts_dir: base_dir.join("artifacts").to_owned(),
            base_dir,
            base_dir_created: false,
            stdout_opened: false,
            stderr_opened: false,
            status_written: false,
            config_hash,
        })
    }

    // Create and return base directory
    fn get_base_dir(&mut self) -> anyhow::Result<&Path> {
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
}

impl TestJobOutput for DatabaseOutput {
    fn stdout(&mut self) -> Result<Stdio> {
        assert!(!self.stdout_opened);
        self.stdout_opened = true;
        Ok(Stdio::from(File::create(
            self.get_base_dir()?.join("stdout.txt"),
        )?))
    }

    fn stderr(&mut self) -> Result<Stdio> {
        assert!(!self.stderr_opened);
        self.stderr_opened = true;
        Ok(Stdio::from(File::create(
            self.get_base_dir()?.join("stderr.txt"),
        )?))
    }

    // TODO: Figure out how to record errors in the more general case, probably with a JSON object.
    fn set_result(&mut self, result: &TestResult) -> anyhow::Result<()> {
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

    fn artifacts_dir(&mut self) -> anyhow::Result<&Path> {
        create_dir_all(&self.artifacts_dir)?;
        Ok(&self.artifacts_dir)
    }
}

// TODO:
// - Test behaviour on already-existing directories
