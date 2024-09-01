use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    fs, iter,
    path::Path,
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, bail, Context as _};
use serde::Deserialize;

use crate::{
    git::{self, PersistentWorktree},
    test,
};

#[derive(Deserialize, Debug)]
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

#[derive(Deserialize, Debug)]
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

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Test {
    name: String,
    command: Command,
    resources: Option<Vec<Resource>>,
    #[serde(default = "default_shutdown_grace_period")]
    /// When a job is no longer needed it's SIGINTed. If it doesn't respond (by
    /// dying) after this duration it will then be SIGKILLed. This also affects
    /// the overall shutdown of local-ci so do not set this to longer than you are
    /// willing to wait when you terminate this program.
    shutdown_grace_period_s: u64,
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
    config_path: &Path,
) -> anyhow::Result<test::ManagerBuilder<PersistentWorktree>> {
    let config_content = fs::read_to_string(config_path).context("couldn't read config")?;
    let config: Config = toml::from_str(&config_content).context("couldn't parse config")?;

    // Build map of resource name to numerical index.
    let resource_idxs: HashMap<String, usize> = config
        .resources
        .as_ref()
        .unwrap_or(&vec![])
        .iter()
        .enumerate()
        .map(|(i, resource)| (resource.name().to_owned(), i))
        .collect();

    let tests = config
        .tests
        .iter()
        .map(|t| {
            let mut needs_resource_idxs: Vec<_> =
                iter::repeat(0).take(resource_idxs.len()).collect();
            let mut seen_resources = HashSet::new();
            for resource in t.resources.as_ref().unwrap_or(&vec![]) {
                if seen_resources.contains(&resource.name()) {
                    // TODO: Need better error messages.
                    bail!("duplicate resource reference {:?}", resource.name());
                }
                seen_resources.insert(resource.name());

                let idx = *resource_idxs
                    .get(resource.name())
                    .ok_or_else(|| anyhow!("undefined resource {:?}", resource.name()))?;
                needs_resource_idxs[idx] = resource.count();
            }
            Ok(test::Test {
                name: t.name.clone(),
                program: t.command.program(),
                args: t.command.args(),
                needs_resource_idxs,
                shutdown_grace_period: Duration::from_secs(t.shutdown_grace_period_s),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // TODO: deduplicate this!
    let mut resource_token_counts: Vec<_> = iter::repeat(0).take(resource_idxs.len()).collect();
    let mut seen_resources = HashSet::new();
    for resource in config.resources.as_ref().unwrap_or(&vec![]) {
        if seen_resources.contains(&resource.name()) {
            // TODO: Need better error messages.
            bail!("duplicate resource reference {:?}", resource.name());
        }
        seen_resources.insert(resource.name());

        let idx = *resource_idxs
            .get(resource.name())
            .ok_or_else(|| anyhow!("undefined resource {:?}", resource.name()))?;
        resource_token_counts[idx] = resource.count();
    }

    Ok(
        test::Manager::builder(repo.clone(), tests, resource_token_counts)
            .num_worktrees(config.num_worktrees),
    )
}
