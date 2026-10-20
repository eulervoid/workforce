//! How the target branch of the principal's checkout stands against the target in wf's bare
//! repository, which agents start from: commits the principal never pushed are invisible to them.
//!
//! The two are compared directly, without a fetch and without writing anything: the checkout's
//! remote-tracking branch of the target may be stale.

use std::path::Path;

use crate::VcsError;
use crate::git::spawn;

/// How the checkout's target branch stands against the target in wf's repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetStanding {
    /// Both are at the same commit.
    Same,
    /// The checkout has `commits` that wf's repository lacks, and lacks none of its.
    Ahead { commits: usize },
    /// wf's repository has `commits` that the checkout lacks, as after merges.
    Behind { commits: usize },
    /// Each has commits the other lacks: a push is refused until the checkout is rebased.
    Diverged,
}

/// How the branch `target` of the checkout at `checkout` stands against `target` in the bare
/// repository at `bare`. `None` when there is nothing to compare: no bare repository yet, or no
/// branch `target` in the checkout. A bare repository without the branch lacks every commit.
///
/// Two git calls when both are at the same commit, and four at most: the comparison runs in
/// whichever repository has both commits.
pub fn target_standing(
    checkout: &Path,
    bare: &Path,
    target: &str,
) -> Result<Option<TargetStanding>, VcsError> {
    if !bare.join("HEAD").is_file() {
        return Ok(None);
    }
    let Some(tip) = branch_tip(checkout, target)? else {
        return Ok(None);
    };
    let Some(target_tip) = branch_tip(bare, target)? else {
        let commits = count(checkout, &tip)?;
        return Ok(Some(TargetStanding::Ahead { commits }));
    };
    if tip == target_tip {
        return Ok(Some(TargetStanding::Same));
    }
    let both = if has_commit(checkout, &target_tip)? {
        checkout
    } else if has_commit(bare, &tip)? {
        bare
    } else {
        return Ok(Some(TargetStanding::Diverged));
    };
    let (ahead, behind) = ahead_and_behind(both, &tip, &target_tip)?;
    Ok(Some(match (ahead, behind) {
        (0, 0) => TargetStanding::Same,
        (commits, 0) => TargetStanding::Ahead { commits },
        (0, commits) => TargetStanding::Behind { commits },
        _ => TargetStanding::Diverged,
    }))
}

/// The commit `branch` points at in the repository at `directory`, if it has the branch.
fn branch_tip(directory: &Path, branch: &str) -> Result<Option<String>, VcsError> {
    let reference = format!("refs/heads/{branch}^{{commit}}");
    let found = spawn(
        directory,
        &[],
        ["rev-parse", "--verify", "--quiet", &reference],
    )?;
    Ok(found.success.then(|| found.stdout.trim().to_string()))
}

/// Whether the repository at `directory` has the commit `commit`.
fn has_commit(directory: &Path, commit: &str) -> Result<bool, VcsError> {
    let object = format!("{commit}^{{commit}}");
    Ok(spawn(directory, &[], ["cat-file", "-e", &object])?.success)
}

/// How many commits `commit` and its ancestors are.
fn count(directory: &Path, commit: &str) -> Result<usize, VcsError> {
    let output = spawn(directory, &[], ["rev-list", "--count", commit])?.ok()?;
    parse_count("rev-list --count", &output)
}

/// How many commits `tip` has that `target_tip` lacks, and the other way round.
fn ahead_and_behind(
    directory: &Path,
    tip: &str,
    target_tip: &str,
) -> Result<(usize, usize), VcsError> {
    let range = format!("{tip}...{target_tip}");
    let command = "rev-list --left-right --count";
    let output = spawn(
        directory,
        &[],
        ["rev-list", "--left-right", "--count", &range],
    )?
    .ok()?;
    let mut counts = output.split_whitespace();
    match (counts.next(), counts.next(), counts.next()) {
        (Some(ahead), Some(behind), None) => {
            Ok((parse_count(command, ahead)?, parse_count(command, behind)?))
        }
        _ => Err(VcsError::UnexpectedOutput {
            command: command.into(),
            output,
        }),
    }
}

fn parse_count(command: &str, text: &str) -> Result<usize, VcsError> {
    text.trim().parse().map_err(|_| VcsError::UnexpectedOutput {
        command: command.into(),
        output: text.trim().into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::GitCli;
    use crate::testing::{TestRepository, git_in};

    /// A checkout on `main` and wf's bare repository made from it, so both start the same.
    fn checkout_and_bare() -> (TestRepository, std::path::PathBuf) {
        let repo = TestRepository::new();
        let bare = repo.root.path().join("wf-git");
        GitCli::create_bare_repository(&bare, "main", Some(&repo.directory())).unwrap();
        (repo, bare)
    }

    fn standing(repo: &TestRepository, bare: &Path) -> Option<TargetStanding> {
        target_standing(&repo.directory(), bare, "main").unwrap()
    }

    /// Commits on `main` in a clone of `bare` and pushes them, as a merge would move the target.
    fn advance_bare(repo: &TestRepository, bare: &Path, commits: usize) {
        let clone = repo.root.path().join("clone");
        if !clone.exists() {
            let (from, to) = (bare.to_str().unwrap(), clone.to_str().unwrap());
            git_in(repo.root.path(), &["clone", "--quiet", from, to]);
            git_in(&clone, &["config", "user.name", "Test"]);
            git_in(&clone, &["config", "user.email", "test@example.com"]);
            git_in(&clone, &["config", "commit.gpgsign", "false"]);
        }
        for index in 0..commits {
            git_in(&clone, &["pull", "--quiet", "--ff-only"]);
            repo.write(&clone, "merged.txt", &format!("{index}\n"));
            repo.commit(&clone, "merged");
            git_in(&clone, &["push", "--quiet", "origin", "main"]);
        }
    }

    fn commit_in_checkout(repo: &TestRepository, name: &str) {
        repo.write(&repo.directory(), name, "local\n");
        repo.commit(&repo.directory(), name);
    }

    #[test]
    fn a_checkout_at_the_target_is_the_same() {
        let (repo, bare) = checkout_and_bare();
        assert_eq!(standing(&repo, &bare), Some(TargetStanding::Same));
    }

    #[test]
    fn commits_never_pushed_put_the_checkout_ahead() {
        let (repo, bare) = checkout_and_bare();
        commit_in_checkout(&repo, "a.txt");
        commit_in_checkout(&repo, "b.txt");
        assert_eq!(
            standing(&repo, &bare),
            Some(TargetStanding::Ahead { commits: 2 })
        );
    }

    #[test]
    fn merges_not_pulled_put_the_checkout_behind_without_a_fetch() {
        let (repo, bare) = checkout_and_bare();
        advance_bare(&repo, &bare, 3);
        assert_eq!(
            standing(&repo, &bare),
            Some(TargetStanding::Behind { commits: 3 })
        );
    }

    #[test]
    fn behind_after_a_fetch_is_counted_in_the_checkout() {
        let (repo, bare) = checkout_and_bare();
        advance_bare(&repo, &bare, 1);
        git_in(&repo.directory(), &["fetch", "--quiet", "wf"]);
        assert_eq!(
            standing(&repo, &bare),
            Some(TargetStanding::Behind { commits: 1 })
        );
    }

    #[test]
    fn commits_on_both_sides_diverge_whichever_repository_has_both() {
        let (repo, bare) = checkout_and_bare();
        advance_bare(&repo, &bare, 1);
        commit_in_checkout(&repo, "a.txt");
        assert_eq!(standing(&repo, &bare), Some(TargetStanding::Diverged));
        git_in(&repo.directory(), &["fetch", "--quiet", "wf"]);
        assert_eq!(standing(&repo, &bare), Some(TargetStanding::Diverged));
    }

    #[test]
    fn an_empty_bare_repository_lacks_every_commit_of_the_checkout() {
        let repo = TestRepository::new();
        let bare = repo.root.path().join("wf-git");
        GitCli::create_bare_repository(&bare, "main", None).unwrap();
        assert_eq!(
            standing(&repo, &bare),
            Some(TargetStanding::Ahead { commits: 1 })
        );
    }

    #[test]
    fn nothing_compares_without_a_bare_repository_or_a_target_branch() {
        let (repo, bare) = checkout_and_bare();
        assert_eq!(
            target_standing(&repo.directory(), &bare, "trunk").unwrap(),
            None
        );
        let missing = repo.root.path().join("no-git");
        assert_eq!(standing(&repo, &missing), None);
    }
}
