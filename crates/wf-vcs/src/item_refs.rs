//! The refs that follow the heads the log names: the branch of an item, and the work of sessions
//! that lost the claim, kept apart from it.

use wf_core::id::{HashId, ItemId, SessionId};
use wf_core::session::CommitId;

use crate::{GitCli, VcsError, item_branch};

/// What the refs of work set apart begin with. They are refs of the bare repository, so the
/// principal's checkout sees them and git does not collect what they keep.
const DIVERGED_PREFIX: &str = "refs/wf/diverged/";

/// The ref the head of `session` is kept under when it lost the claim on its item and is not in
/// the branch.
pub fn diverged_ref(session: SessionId) -> String {
    format!("{DIVERGED_PREFIX}{}", session.to_base32())
}

impl GitCli {
    /// Moves the branch of `item` from `expected` (`None`: it does not exist) to `commit`, unless
    /// something else moved it meanwhile. Returns whether it moved.
    pub fn move_item_branch(
        &self,
        item: ItemId,
        expected: Option<CommitId>,
        commit: CommitId,
    ) -> Result<bool, VcsError> {
        self.move_branch(&item_branch(item), expected, commit)
    }

    /// Moves `branch` from `expected` (`None`: it does not exist) to `commit`, unless something
    /// else moved it meanwhile. Returns whether it moved.
    pub(crate) fn move_branch(
        &self,
        branch: &str,
        expected: Option<CommitId>,
        commit: CommitId,
    ) -> Result<bool, VcsError> {
        let reference = format!("refs/heads/{branch}");
        let old = expected.map_or_else(String::new, |expected| expected.to_string());
        let moved = self.run(
            &self.repository,
            &[],
            ["update-ref", &reference, &commit.to_string(), &old],
        )?;
        if moved.success {
            return Ok(true);
        }
        // A branch that is not where it was expected is the answer; anything else is an error.
        let now = match self.branch_head(branch) {
            Ok(now) => Some(now),
            Err(VcsError::NoBranch(_)) => None,
            Err(error) => return Err(error),
        };
        if now == expected {
            moved.ok()?;
        }
        Ok(false)
    }

    /// Makes the branch of `item` contain `head`: moves it there when it is missing or does not
    /// reach it, and leaves it when it is at `head` or ahead. A branch moved by someone else
    /// meanwhile is looked at again.
    pub fn bring_item_branch_to(&self, item: ItemId, head: CommitId) -> Result<(), VcsError> {
        loop {
            let branch = match self.branch_head(&item_branch(item)) {
                Ok(branch) => Some(branch),
                Err(VcsError::NoBranch(_)) => None,
                Err(error) => return Err(error),
            };
            if let Some(branch) = branch
                && self.is_ancestor(head, branch)?
            {
                return Ok(());
            }
            if self.move_item_branch(item, branch, head)? {
                return Ok(());
            }
        }
    }

    /// The branches of items, by name (`wf/<item>`).
    pub fn item_branches(&self) -> Result<Vec<String>, VcsError> {
        let listed = self
            .run(
                &self.repository,
                &[],
                [
                    "for-each-ref",
                    "--format=%(refname:short)",
                    "refs/heads/wf/",
                ],
            )?
            .ok()?;
        Ok(listed.lines().map(str::to_string).collect())
    }

    /// Keeps `commit` under the ref of `session`'s work set apart.
    pub fn keep_apart(&self, session: SessionId, commit: CommitId) -> Result<(), VcsError> {
        self.update_ref(&diverged_ref(session), commit)
    }

    /// Removes the ref of `session`'s work set apart, if there is one.
    pub fn drop_apart(&self, session: SessionId) -> Result<(), VcsError> {
        self.run(
            &self.repository,
            &[],
            ["update-ref", "-d", &diverged_ref(session)],
        )?
        .ok()?;
        Ok(())
    }

    /// The refs of work set apart, by name.
    pub fn refs_apart(&self) -> Result<Vec<String>, VcsError> {
        let listed = self
            .run(
                &self.repository,
                &[],
                ["for-each-ref", "--format=%(refname)", DIVERGED_PREFIX],
            )?
            .ok()?;
        Ok(listed.lines().map(str::to_string).collect())
    }

    /// Whether `ancestor` is `descendant` or one of its ancestors. Both must be in this
    /// repository.
    pub fn is_ancestor(&self, ancestor: CommitId, descendant: CommitId) -> Result<bool, VcsError> {
        let (ancestor, descendant) = (ancestor.to_string(), descendant.to_string());
        let found = self.run(
            &self.repository,
            &[],
            ["merge-base", "--is-ancestor", &ancestor, &descendant],
        )?;
        Ok(found.success)
    }

    fn update_ref(&self, reference: &str, commit: CommitId) -> Result<(), VcsError> {
        self.run(
            &self.repository,
            &[],
            ["update-ref", reference, &commit.to_string()],
        )?
        .ok()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{TestRepository, git_in};

    #[test]
    fn work_set_apart_is_kept_under_a_ref_and_removed_again() {
        let repository = TestRepository::new();
        let directory = repository.directory();
        let first = repository.commit_id("HEAD", &directory);
        repository.write(&directory, "file.txt", "changed\n");
        let second = repository.commit(&directory, "second");
        let session = SessionId::from_bytes([7; 32]);
        let git = &repository.git;

        assert_eq!(git.refs_apart().unwrap(), Vec::<String>::new());
        git.keep_apart(session, first).unwrap();
        assert_eq!(git.refs_apart().unwrap(), vec![diverged_ref(session)]);
        assert_eq!(
            git_in(&directory, &["rev-parse", &diverged_ref(session)]),
            first.to_string()
        );
        git.drop_apart(session).unwrap();
        git.drop_apart(session).unwrap();
        assert!(git.refs_apart().unwrap().is_empty());

        assert!(git.is_ancestor(first, second).unwrap());
        assert!(git.is_ancestor(first, first).unwrap());
        assert!(!git.is_ancestor(second, first).unwrap());
    }

    #[test]
    fn the_branch_of_an_item_moves_only_from_where_it_was_expected() {
        let repository = TestRepository::new();
        let directory = repository.directory();
        let first = repository.commit_id("HEAD", &directory);
        repository.write(&directory, "file.txt", "changed\n");
        let second = repository.commit(&directory, "second");
        let item = ItemId::from_bytes([3; 32]);
        let git = &repository.git;
        assert!(git.move_item_branch(item, None, first).unwrap());
        assert_eq!(git.branch_head(&item_branch(item)).unwrap(), first);
        assert!(git.move_item_branch(item, Some(first), second).unwrap());
        assert_eq!(git.branch_head(&item_branch(item)).unwrap(), second);

        // Someone else got there first: nothing moves.
        assert!(!git.move_item_branch(item, Some(first), first).unwrap());
        assert!(!git.move_item_branch(item, None, first).unwrap());
        assert_eq!(git.branch_head(&item_branch(item)).unwrap(), second);
        let other = ItemId::from_bytes([4; 32]);
        assert!(!git.move_item_branch(other, Some(first), second).unwrap());
        assert!(git.branch_head(&item_branch(other)).is_err());
    }

    #[test]
    fn the_branch_is_brought_to_a_head_only_when_it_does_not_reach_it() {
        let repository = TestRepository::new();
        let directory = repository.directory();
        let first = repository.commit_id("HEAD", &directory);
        repository.write(&directory, "file.txt", "changed\n");
        let second = repository.commit(&directory, "second");
        let item = ItemId::from_bytes([3; 32]);
        let git = &repository.git;
        git.bring_item_branch_to(item, first).unwrap();
        assert_eq!(git.branch_head(&item_branch(item)).unwrap(), first);
        git.bring_item_branch_to(item, second).unwrap();
        assert_eq!(git.branch_head(&item_branch(item)).unwrap(), second);
        git.bring_item_branch_to(item, first).unwrap();
        assert_eq!(
            git.branch_head(&item_branch(item)).unwrap(),
            second,
            "a branch ahead stays"
        );
    }

    #[test]
    fn missing_commits_are_told_from_present_ones_in_one_run() {
        let repository = TestRepository::new();
        let directory = repository.directory();
        let first = repository.commit_id("HEAD", &directory);
        let unknown = CommitId::Git(wf_core::session::GitOid::from_bytes([9; 20]));
        let git = &repository.git;
        assert_eq!(git.missing_commits(&[]).unwrap(), vec![]);
        assert_eq!(
            git.missing_commits(&[first, unknown, first]).unwrap(),
            vec![unknown]
        );
    }
}
