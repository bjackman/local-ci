use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs,
    hash::{DefaultHasher, Hash as _, Hasher as _},
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{bail, Context as _};
use serde::Deserialize;

use crate::{
    git::{self, PersistentWorktree},
    resource::ResourceKey,
    result::Database,
    test::{self, CachePolicy, TestName},
};

#[derive(Deserialize, Debug, Hash)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum Resource {
    Bare(String),
    Counted { name: String, count: usize },
}

impl Resource {
    pub fn name(&self) -> &str {
        match self {
            Self::Bare(n) => n,
            Self::Counted { name: n, count: _ } => n,
        }
    }

    pub fn count(&self) -> usize {
        match self {
            Self::Bare(_) => 1,
            Self::Counted { name: _, count: c } => *c,
        }
    }
}

#[derive(Deserialize, Debug, Hash)]
#[serde(deny_unknown_fields)]
#[serde(untagged)]
pub enum Command {
    Shell(String),
    Raw(Vec<String>),
}

impl Command {
    pub fn program(&self) -> OsString {
        match self {
            Self::Shell(_) => "bash".into(), // TODO: Figure out the user's configured shell.
            Self::Raw(args) => args[0].clone().into(),
        }
    }

    pub fn args(&self) -> Vec<OsString> {
        match self {
            Self::Shell(cmd) => vec!["-c".into(), cmd.into()],
            Self::Raw(args) => args[1..].iter().map(|s| s.into()).collect(),
        }
    }
}

#[derive(Deserialize, Debug, Hash)]
#[serde(deny_unknown_fields)]
pub struct Test {
    name: String,
    command: Command,
    #[serde(default = "default_requires_worktree")]
    requires_worktree: bool,
    resources: Option<Vec<Resource>>,
    #[serde(default = "default_shutdown_grace_period")]
    /// When a job is no longer needed it's SIGINTed. If it doesn't respond (by
    /// dying) after this duration it will then be SIGKILLed. This also affects
    /// the overall shutdown of local-ci so do not set this to longer than you are
    /// willing to wait when you terminate this program.
    shutdown_grace_period_s: u64,
    #[serde(default = "default_cache_policy")]
    cache: CachePolicy,
    #[serde(default = "default_depends_on")]
    depends_on: Vec<String>,
}

fn default_depends_on() -> Vec<String> {
    vec![]
}

fn default_requires_worktree() -> bool {
    true
}

impl Test {
    // Convert to the "real" object.
    pub fn parse(&self) -> anyhow::Result<test::Test> {
        let mut seen_resources = HashSet::new();
        for resource in self.resources.as_ref().unwrap_or(&vec![]) {
            if seen_resources.contains(&resource.name()) {
                // TODO: Need better error messages.
                bail!("duplicate resource reference {:?}", resource.name());
            }
            seen_resources.insert(resource.name());
        }
        let mut needs_resources: HashMap<ResourceKey, usize> = self
            .resources
            .as_ref()
            .unwrap_or(&vec![])
            .iter()
            .map(|r| (ResourceKey::UserToken(r.name().to_owned()), r.count()))
            .collect();
        if self.requires_worktree {
            needs_resources.insert(ResourceKey::Worktree, 1);
        }
        Ok(test::Test {
            name: TestName::new(self.name.clone()),
            program: self.command.program(),
            args: self.command.args(),
            needs_resources,
            shutdown_grace_period: Duration::from_secs(self.shutdown_grace_period_s),
            cache_policy: self.cache,
            config_hash: {
                let mut h = DefaultHasher::new();
                self.hash(&mut h);
                h.finish()
            },
            depends_on: self.depends_on.iter().map(TestName::new).collect(),
        })
    }
}

fn default_cache_policy() -> CachePolicy {
    // Hard to choose a default here. Rationale for this choice: It's weird not
    // to want any caching at all. Almost all of the time you want ByTree, but
    // ByCommit will give you 80% of the value, and lots of people don't think
    // about the difference between tree and commit anyway.
    CachePolicy::ByCommit
}

fn default_shutdown_grace_period() -> u64 {
    10
}

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    num_worktrees: usize,
    resources: Option<Vec<Resource>>,
    tests: Vec<Test>,
}

pub fn manager_builder(
    repo: Arc<git::PersistentWorktree>,
    cache_path: &Path,
    config_path: &Path,
) -> anyhow::Result<test::ManagerBuilder<PersistentWorktree>> {
    let config_content = fs::read_to_string(config_path).context("couldn't read config")?;
    let config: Config = toml::from_str(&config_content).context("couldn't parse config")?;

    // Build map of resource name to numerical index.
    let resource_tokens: HashMap<ResourceKey, Vec<String>> = config
        .resources
        .as_ref()
        .unwrap_or(&vec![])
        .iter()
        .map(|resource| {
            (
                ResourceKey::UserToken(resource.name().to_owned()),
                // Here we'll eventually allow the user to name the resources
                // explicitly. For now we just pick a unique name.
                (0..resource.count())
                    .map(|i| format!("{}-{}", resource.name(), i))
                    .collect(),
            )
        })
        .collect();

    // Parse all the tests, with reference to the named resource idxs.
    let tests = config
        .tests
        .iter()
        .map(|t| t.parse())
        .collect::<anyhow::Result<Vec<_>>>()?;

    // Check for invalid resource references.
    for test in tests.iter() {
        for key in test.needs_resources.keys() {
            if let ResourceKey::UserToken(name) = key {
                if !resource_tokens.contains_key(key) {
                    bail!(
                        "undefined resource {:?} referenced in test {:?}",
                        name,
                        test.name
                    );
                }
            }
        }
    }

    Ok(test::Manager::builder(
        repo.clone(),
        Database::create_or_open(cache_path)?,
        tests,
        resource_tokens,
    )
    .num_worktrees(config.num_worktrees))
}
