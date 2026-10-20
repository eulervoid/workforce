//! The refs that follow the heads the log names: the branch of an item, and the work of sessions
//! that lost the claim, kept apart from it.

use wf_core::id::{HashId, ItemId, SessionId};
use wf_core::session::CommitId;

use crate::{GitCli, VcsError, item_branch};

/// What the refs of work set apart begin with. They are refs of the bare repository, so a commit
/// kept under one is served to other devices like one on a branch.
const DIVERGED_PREFIX: &str = "refs/wf/diverged/";

/// The ref the head of `session` is kept under when it lost the claim on its item and is not in
/// the branch.
pub fn diverged_ref(session: SessionId) -> String {
    format!("{DIVERGED_PREFIX}{}", session.to_base32())
}

impl GitCli {
    /// Moves the branch of `item` to `commit`, whatever it was at.
    pub fn set_item_branch(&self, item: ItemId, commit: CommitId) -> Result<(), VcsError> {
        let reference = format!("refs/heads/{}", item_branch(item));
        self.update_ref(&reference, commit)
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
    fn the_branch_of_an_item_moves_to_the_commit_named() {
        let repository = TestRepository::new();
        let directory = repository.directory();
        let first = repository.commit_id("HEAD", &directory);
        repository.write(&directory, "file.txt", "changed\n");
        let second = repository.commit(&directory, "second");
        let item = ItemId::from_bytes([3; 32]);
        let git = &repository.git;
        git.set_item_branch(item, first).unwrap();
        assert_eq!(git.branch_head(&item_branch(item)).unwrap(), first);
        git.set_item_branch(item, second).unwrap();
        assert_eq!(git.branch_head(&item_branch(item)).unwrap(), second);
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
