//! How the target branch of the principal's checkout stands against the target in wf's bare
//! repository, which agents start from: commits the principal never pushed are invisible to them.
//!
//! The two are compared directly, without a fetch and without writing anything: the checkout's
//! remote-tracking branch of the target may be stale.
//!
//! Whichever side is behind is moved by a fast-forward of the other, and only by one: a push that
//! is never forced, and a fast-forward of the checkout that never touches uncommitted changes.
//! Diverged branches stay as they are, for the principal to rebase.

use std::path::Path;

use crate::git::spawn;
use crate::{REMOTE, VcsError};

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

/// Pushes the branch `target` of the checkout at `checkout` to the bare repository at `bare`, as
/// `git push wf <target>` does, if that fast-forwards it: a push that does not is refused, never
/// forced. The checkout's remote-tracking branch of the target follows.
pub fn push_target(checkout: &Path, bare: &Path, target: &str) -> Result<(), VcsError> {
    let reference = format!("refs/heads/{target}");
    let refspec = format!("{reference}:{reference}");
    let bare_text = bare.to_string_lossy();
    let args = ["push", "--quiet", &bare_text, &refspec];
    spawn(checkout, &[], args)?.ok()?;
    fetch_tracking_branch(checkout, bare, target)
}

/// Fast-forwards the branch `target` of the checkout at `checkout` to `target` of the bare
/// repository at `bare`, and fails if that is no fast-forward. When the checkout has the branch
/// checked out, its files move with it as `git merge --ff-only` moves them, which refuses to touch
/// uncommitted changes. Otherwise the branch alone moves, unless another worktree of the checkout
/// has it checked out or a rebase or bisect there works on it.
pub fn fast_forward_target(checkout: &Path, bare: &Path, target: &str) -> Result<(), VcsError> {
    fetch_tracking_branch(checkout, bare, target)?;
    let reference = format!("refs/heads/{target}");
    let tracking = format!("refs/remotes/{REMOTE}/{target}");
    let head = spawn(checkout, &[], ["symbolic-ref", "--quiet", "HEAD"])?;
    if head.success && head.stdout.trim() == reference {
        spawn(
            checkout,
            &[],
            ["merge", "--ff-only", "--quiet", "--no-stat", &tracking],
        )?
        .ok()?;
    } else {
        // A fetch into a branch only fast-forwards it, from the tip it read, and refuses a branch
        // that any worktree has checked out or is rebasing or bisecting.
        let refspec = format!("{tracking}:{reference}");
        let args = [
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            ".",
            &refspec,
        ];
        spawn(checkout, &[], args)?.ok()?;
    }
    Ok(())
}

/// Sets the checkout's remote-tracking branch of `target` to where it is in the bare repository,
/// as `git fetch wf` does, and brings its commits along.
fn fetch_tracking_branch(checkout: &Path, bare: &Path, target: &str) -> Result<(), VcsError> {
    let refspec = format!("+refs/heads/{target}:refs/remotes/{REMOTE}/{target}");
    let bare_text = bare.to_string_lossy();
    let args = [
        "fetch",
        "--quiet",
        "--no-tags",
        "--no-write-fetch-head",
        &bare_text,
        &refspec,
    ];
    spawn(checkout, &[], args)?.ok()?;
    Ok(())
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

    fn push(repo: &TestRepository, bare: &Path) -> Result<(), VcsError> {
        push_target(&repo.directory(), bare, "main")
    }

    fn fast_forward(repo: &TestRepository, bare: &Path) -> Result<(), VcsError> {
        fast_forward_target(&repo.directory(), bare, "main")
    }

    fn tip(directory: &Path, reference: &str) -> String {
        git_in(directory, &["rev-parse", reference])
    }

    #[test]
    fn a_checkout_ahead_is_pushed_and_its_tracking_branch_follows() {
        let (repo, bare) = checkout_and_bare();
        commit_in_checkout(&repo, "a.txt");
        commit_in_checkout(&repo, "b.txt");
        push(&repo, &bare).unwrap();
        assert_eq!(standing(&repo, &bare), Some(TargetStanding::Same));
        let checkout = repo.directory();
        assert_eq!(tip(&bare, "main"), tip(&checkout, "main"));
        assert_eq!(tip(&checkout, "wf/main"), tip(&checkout, "main"));
    }

    #[test]
    fn a_checkout_behind_with_the_target_checked_out_is_fast_forwarded_with_its_files() {
        let (repo, bare) = checkout_and_bare();
        advance_bare(&repo, &bare, 2);
        fast_forward(&repo, &bare).unwrap();
        assert_eq!(standing(&repo, &bare), Some(TargetStanding::Same));
        let checkout = repo.directory();
        assert_eq!(
            std::fs::read_to_string(checkout.join("merged.txt")).unwrap(),
            "1\n"
        );
        assert_eq!(git_in(&checkout, &["status", "--porcelain"]), "");
    }

    #[test]
    fn uncommitted_changes_the_fast_forward_would_touch_keep_the_checkout_where_it_is() {
        let (repo, bare) = checkout_and_bare();
        advance_bare(&repo, &bare, 1);
        let checkout = repo.directory();
        repo.write(&checkout, "merged.txt", "mine\n");
        let before = tip(&checkout, "main");
        assert!(fast_forward(&repo, &bare).is_err());
        assert_eq!(tip(&checkout, "main"), before);
        assert_eq!(
            std::fs::read_to_string(checkout.join("merged.txt")).unwrap(),
            "mine\n"
        );
        assert_eq!(
            standing(&repo, &bare),
            Some(TargetStanding::Behind { commits: 1 })
        );
    }

    #[test]
    fn uncommitted_changes_the_fast_forward_leaves_alone_stay_as_they_are() {
        let (repo, bare) = checkout_and_bare();
        advance_bare(&repo, &bare, 1);
        let checkout = repo.directory();
        repo.write(&checkout, "file.txt", "mine\n");
        fast_forward(&repo, &bare).unwrap();
        assert_eq!(standing(&repo, &bare), Some(TargetStanding::Same));
        assert_eq!(
            std::fs::read_to_string(checkout.join("file.txt")).unwrap(),
            "mine\n"
        );
    }

    #[test]
    fn a_target_not_checked_out_moves_alone() {
        let (repo, bare) = checkout_and_bare();
        advance_bare(&repo, &bare, 1);
        let checkout = repo.directory();
        git_in(&checkout, &["switch", "--quiet", "-c", "own"]);
        repo.write(&checkout, "merged.txt", "mine\n");
        fast_forward(&repo, &bare).unwrap();
        assert_eq!(standing(&repo, &bare), Some(TargetStanding::Same));
        assert_eq!(
            git_in(&checkout, &["symbolic-ref", "--short", "HEAD"]),
            "own"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("merged.txt")).unwrap(),
            "mine\n"
        );
    }

    #[test]
    fn a_target_checked_out_in_another_worktree_is_left_alone() {
        let (repo, bare) = checkout_and_bare();
        advance_bare(&repo, &bare, 1);
        let checkout = repo.directory();
        git_in(&checkout, &["switch", "--quiet", "-c", "own"]);
        let other = repo.root.path().join("other");
        git_in(
            &checkout,
            &[
                "worktree",
                "add",
                "--quiet",
                other.to_str().unwrap(),
                "main",
            ],
        );
        assert!(fast_forward(&repo, &bare).is_err());
        assert_eq!(
            standing(&repo, &bare),
            Some(TargetStanding::Behind { commits: 1 })
        );
    }

    #[test]
    fn diverged_branches_are_neither_pushed_nor_fast_forwarded() {
        let (repo, bare) = checkout_and_bare();
        advance_bare(&repo, &bare, 1);
        commit_in_checkout(&repo, "a.txt");
        let (checkout, bare_tip) = (tip(&repo.directory(), "main"), tip(&bare, "main"));
        assert!(push(&repo, &bare).is_err());
        assert!(fast_forward(&repo, &bare).is_err());
        assert_eq!(tip(&repo.directory(), "main"), checkout);
        assert_eq!(tip(&bare, "main"), bare_tip);
        assert_eq!(standing(&repo, &bare), Some(TargetStanding::Diverged));

        git_in(&repo.directory(), &["switch", "--quiet", "-c", "own"]);
        assert!(fast_forward(&repo, &bare).is_err(), "nor the branch alone");
        assert_eq!(tip(&repo.directory(), "main"), checkout);
    }
}
