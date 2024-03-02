use anyhow::{anyhow, Context};

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

// This module contains horribly manual git logic. This is manual for two main reasons:
// - We need to be able to get notified of changes to ranges, this is not something that git
//   natively supports so we actually need to peek at .git.
// - We want cancellation on operations like checkout, since that can take some time on large repos.
//   The Git CLI supports this but libraries don't. The Git CLI is actually Git's only properly
//   supported "API" anyway I believe.

pub struct Repo {
    git_dir: PathBuf,
}

impl Repo {
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let mut git_file = File::open(&path.join(".git"))?;
        if git_file.metadata()?.file_type().is_dir() {
            return Ok(Repo { git_dir: path });
        }

        fn strip_newline(b: &[u8]) -> &[u8] {
            b.strip_suffix("\n".as_bytes()).unwrap_or(b)
        }

        // .git is not a directory. Is it a worktree pointer? That's a file that looks like
        // "gitdir: /path/to/gitdir"
        let mut content = Vec::new();
        git_file.read_to_end(&mut content)?;
        let path = match content.strip_prefix("gitdir: ".as_bytes()) {
            None => return Err(anyhow!(".git text file didn't start with 'gitdir: '")),
            Some(suffix) => PathBuf::from(OsStr::from_bytes(strip_newline(suffix))),
        };
        // It should be a subdir of the original .git dir, named "worktrees/$name".
        let worktrees_path = path.parent().ok_or(anyhow!(format!(
            "{:?} not a worktree path (no parent)",
            path
        )))?;
        if worktrees_path.file_name() != Some(&OsStr::new("worktrees")) {
            return Err(anyhow!(format!("{:?} not a worktrees path", path)))?;
        }
        let git_path = worktrees_path.parent().ok_or(anyhow!(format!(
            "{:?} not a worktree path (no parent)",
            path
        )))?;
        let git_file = File::open(&git_path).context(format!("open worktree origin {:?}", path))?;
        if !git_file.metadata()?.file_type().is_dir() {
            return Err(anyhow!(format!("not a git repository: {:?}", path)));
        }
        Ok(Repo {
            git_dir: PathBuf::from(git_path),
        })
    }
}

type RevSpec = OsString;

#[derive(PartialEq, Debug)]
/// Represents a git revision range specification. Note that just becuase the spec could be parsed,
/// doesn't mean that this is actually a valid range in any given repository.
pub struct RangeSpec {
    include: Vec<RevSpec>,
    exclude: Vec<RevSpec>,
}

// RevSpecs are not utf-8. Would like to just represent them as OsString but those lack useful
// manipulation methods. Here we add them.
trait OsStrExt2 {
    // Ideally the params here would be generic as they are for the str methods. For now we just
    // only implement the types we actually need.

    // TODO: Is there a way to write this that doesn't require constructing a whole new Vec?
    fn split(&self, s: u8) -> Vec<&OsStr>;
    // Uses a naïve algorithm that will probably perform unnecessarily badly if s is large.
    fn split2(&self, s: &str) -> Option<(&OsStr, &OsStr)>;
    fn strip_prefix(&self, s: &str) -> Option<&OsStr>;
}

impl OsStrExt2 for OsStr {
    fn split(&self, s: u8) -> Vec<&OsStr> {
        self.as_bytes()
            .split(|b| *b == u8::from(s))
            .map(OsStr::from_bytes)
            .collect()
    }

    fn split2(&self, s: &str) -> Option<(&OsStr, &OsStr)> {
        let haystack = self.as_bytes();
        let needle = s.as_bytes();
        for i in 0..(haystack.len() - (needle.len() - 1)) {
            if haystack[i..i + needle.len()] == *needle {
                return Some((
                    OsStr::from_bytes(&haystack[0..i]),
                    OsStr::from_bytes(&haystack[i + needle.len()..]),
                ));
            }
        }
        return None;
    }

    fn strip_prefix(&self, s: &str) -> Option<&OsStr> {
        match self.as_bytes().strip_prefix(s.as_bytes()) {
            None => None,
            Some(suffix) => Some(OsStr::from_bytes(suffix)),
        }
    }
}

impl RangeSpec {
    fn parse(s: &OsStr) -> anyhow::Result<Self> {
        let mut include = vec![];
        let mut exclude = vec![];

        for part in s.split(b' ') {
            if part.is_empty() {
                continue;
            }

            if let Some((from, to)) = part.split2("..") {
                if from.is_empty() || to.is_empty() {
                    return Err(anyhow!("empty revision in range {:?}", part));
                }
                if to.as_bytes()[0] == b'.' {
                    // We saw a third dot after the "..". We don't implement this syntax. This could
                    // be implemented fairly easily, it just doesn't seem very useful.
                    return Err(anyhow!(
                        "rev spec {:?} - symmetric difference ranges not supported. \
                        Did you mean '..' instead of '...'? See gitrevisions(7)",
                        part
                    ));
                }

                include.push(to.to_os_string());
                exclude.push(from.to_os_string());
                continue;
            }

            match part.strip_prefix("^") {
                None => include.push(part.to_os_string()),
                Some(suffix) => exclude.push(suffix.to_os_string()),
            };
        }

        Ok(RangeSpec { include, exclude })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::process::CommandExt;
    use cancellation_token::CancellationToken;
    use std::io::Write;
    use std::path::Path;
    use std::process;
    use tempfile::TempDir;

    #[test]
    fn test_new_gitdir_notgit() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        assert!(
            Repo::open(tmp_dir.path().to_path_buf()).is_err(),
            "opening repo with no .git didn't fail"
        );
    }

    #[test]
    fn test_new_gitdir_file_notgit() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        {
            let mut bogus_git_file =
                File::create(tmp_dir.path().join(".git")).expect("couldn't create .git");
            write!(bogus_git_file, "no no no").expect("couldn't write .git");
        }
        assert!(
            Repo::open(tmp_dir.path().to_path_buf()).is_err(),
            "opening repo with bogus .git file didn't fail"
        );
    }

    fn must_git<I, S>(path: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = process::Command::new("git");
        cmd.arg("-C").arg(path).args(args);
        cmd.output_ok(&CancellationToken::new(false))
            .expect("git command failed");
    }

    #[test]
    fn test_new_gitdir() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        must_git(tmp_dir.path(), ["init"]);
        let repo = Repo::open(tmp_dir.path().to_path_buf()).expect("failed to open repo");
        assert_eq!(repo.git_dir, tmp_dir.path());
    }

    #[test]
    fn test_new_gitdir_worktree() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        let worktree = TempDir::new().expect("couldn't make worktree tempdir");
        println!("tmp_dir {:?} worktree {:?}", tmp_dir, worktree);
        must_git(tmp_dir.path(), ["init"]);
        must_git(tmp_dir.path(), ["commit", "--allow-empty", "-m", "foo"]);
        let mut cmd = process::Command::new("git");
        cmd.arg("-C")
            .arg(tmp_dir.path())
            .args(["worktree", "add"])
            .arg(worktree.path())
            .arg("HEAD");
        cmd.output_ok(&CancellationToken::new(false))
            .expect("couldn't setup git worktree");
        let repo = Repo::open(worktree.path().to_path_buf()).expect("failed to open repo");
        assert_eq!(repo.git_dir, tmp_dir.path().join(".git"));
    }

    #[test]
    fn test_revspec_parse() {
        for (spec, want) in [
            (
                "foo",
                RangeSpec {
                    include: ["foo"].map(OsString::from).to_vec(),
                    exclude: vec![],
                },
            ),
            (
                "foo bar",
                RangeSpec {
                    include: ["foo", "bar"].map(OsString::from).to_vec(),
                    exclude: vec![],
                },
            ),
            (
                "foo ^bar ^baz",
                RangeSpec {
                    include: ["foo"].map(OsString::from).to_vec(),
                    exclude: ["bar", "baz"].map(OsString::from).to_vec(),
                },
            ),
            (
                "foo..bar",
                RangeSpec {
                    include: ["bar"].map(OsString::from).to_vec(),
                    exclude: ["foo"].map(OsString::from).to_vec(),
                },
            ),
            (
                "foo..bar baz ^bam",
                RangeSpec {
                    include: ["bar", "baz"].map(OsString::from).to_vec(),
                    exclude: ["foo", "bam"].map(OsString::from).to_vec(),
                },
            ),
        ] {
            assert_eq!(
                RangeSpec::parse(&OsString::from(spec))
                    .expect(&format!("failed to parse {:?} as RevSpec", spec)),
                want,
                "for input string {:?}",
                spec,
            );
        }
    }

    #[test]
    fn test_revspec_parse_error() {
        for (string, want_msg) in [
            ("..", "empty revision in range \"..\""),
            ("f..", "empty revision in range \"f..\""),
            ("..f", "empty revision in range \"..f\""),
            (
                "foo...bar",
                "rev spec \"foo...bar\" - symmetric difference ranges not supported. \
                        Did you mean '..' instead of '...'? See gitrevisions(7)",
            ),
        ] {
            match RangeSpec::parse(&OsString::from(string)) {
                Ok(v) => panic!(
                    "input string {:?} was parsed as {:?}, expected error",
                    string, v
                ),
                Err(error) => {
                    assert_eq!(error.to_string(), want_msg, "for input string {:?}", string)
                }
            }
        }
    }
}
