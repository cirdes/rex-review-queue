//! Worktrees de review: criar e remover com git, sem passar pelo multiplexador.

use crate::git::Git;
use crate::runner::Runner;
use anyhow::Result;
use std::path::Path;

pub trait Worktrees {
    /// Cria `path` numa branch nova `branch` a partir de `base` (`git worktree add -q -b`).
    fn create(&self, repo: &Path, branch: &str, base: &str, path: &Path) -> Result<()>;
    /// Remove a worktree sem `--force` e faz o prune. Uma worktree que já não existe conta como removida.
    fn remove(&self, repo: &Path, path: &Path) -> Result<()>;
}

pub struct GitWorktrees<'a> {
    runner: &'a dyn Runner,
}

impl<'a> GitWorktrees<'a> {
    pub fn new(runner: &'a dyn Runner) -> Self {
        Self { runner }
    }
}

impl Worktrees for GitWorktrees<'_> {
    fn create(&self, repo: &Path, branch: &str, base: &str, path: &Path) -> Result<()> {
        Git::new(self.runner)
            .ok(
                repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    branch,
                    &path.to_string_lossy(),
                    base,
                ],
            )
            .map(|_| ())
    }

    fn remove(&self, repo: &Path, path: &Path) -> Result<()> {
        let git = Git::new(self.runner);
        if path.exists() {
            git.worktree_remove(repo, path)?;
        }
        git.worktree_prune(repo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::RealRunner;
    use std::process::Command;

    fn sh(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        sh(dir.path(), &["init", "-q", "-b", "main"]);
        sh(
            dir.path(),
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        dir
    }

    #[test]
    fn creates_on_a_new_branch_and_removes_with_prune() {
        let r = repo();
        let wt = r.path().join("wt");
        let w = GitWorktrees::new(&RealRunner);
        w.create(r.path(), "feat", "main", &wt).unwrap();
        assert!(wt.join(".git").exists());
        let head = Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["branch", "--show-current"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "feat");
        w.remove(r.path(), &wt).unwrap();
        assert!(!wt.exists());
        let list = Command::new("git")
            .arg("-C")
            .arg(r.path())
            .args(["worktree", "list"])
            .output()
            .unwrap();
        assert!(!String::from_utf8_lossy(&list.stdout).contains("wt"));
    }

    #[test]
    fn removing_a_missing_worktree_is_done() {
        let r = repo();
        GitWorktrees::new(&RealRunner)
            .remove(r.path(), &r.path().join("nada"))
            .unwrap();
    }

    #[test]
    fn remove_refuses_local_changes() {
        let r = repo();
        let wt = r.path().join("wt");
        let w = GitWorktrees::new(&RealRunner);
        w.create(r.path(), "feat", "main", &wt).unwrap();
        std::fs::write(wt.join("novo.txt"), "x").unwrap();
        assert!(
            w.remove(r.path(), &wt).is_err(),
            "sem --force, arquivo não rastreado barra a remoção"
        );
        assert!(wt.exists());
    }
}
