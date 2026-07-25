//! Branch lookup by reading `.git/HEAD` directly.
//!
//! Shelling out to `git rev-parse` once per session per refresh is needless
//! process churn when the answer is a single small file. Worktrees keep a `.git`
//! *file* pointing at the real gitdir, so we follow that one indirection.

use std::fs;
use std::path::{Path, PathBuf};

/// Walk up from `start` until a directory contains a `.git` entry.
pub fn repo_root(start: &Path) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// Resolve `path` to the *main* repository it belongs to, following a worktree
/// back to its parent.
///
/// A linked worktree's `.git` file reads `gitdir: /main/repo/.git/worktrees/<name>`,
/// so everything before the `.git` component is the repo the worktree came from.
/// This works whether the session relocated into the worktree itself or was
/// launched there by hand, which is what makes grouping reliable.
pub fn main_repo_root(path: &Path) -> Option<PathBuf> {
    let root = repo_root(path)?;
    let git = root.join(".git");
    if git.is_dir() {
        return Some(root);
    }

    let contents = fs::read_to_string(&git).ok()?;
    let gitdir = PathBuf::from(contents.strip_prefix("gitdir:")?.trim());
    let comps: Vec<_> = gitdir.components().collect();
    let idx = comps.iter().position(|c| c.as_os_str() == ".git")?;
    let main: PathBuf = comps[..idx].iter().collect();
    // A relative gitdir would resolve to nonsense; keep the worktree in that case.
    if !main.is_absolute() {
        return Some(root);
    }
    // git records the gitdir fully resolved, so a project reached through a
    // symlink would otherwise never match its own worktree's parent.
    Some(canonical(&main))
}

/// Resolve symlinks, falling back to the path as given when that is not possible
/// (a directory that has since been removed, say).
pub fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Current branch for the repo containing `path`, or `None` on detached HEAD.
pub fn branch(path: &Path) -> Option<String> {
    let git = repo_root(path)?.join(".git");
    let head = if git.is_dir() {
        git.join("HEAD")
    } else {
        // Worktree: `.git` is a file reading `gitdir: /abs/path/to/.git/worktrees/<name>`
        let contents = fs::read_to_string(&git).ok()?;
        let gitdir = contents.strip_prefix("gitdir:")?.trim();
        PathBuf::from(gitdir).join("HEAD")
    };

    let contents = fs::read_to_string(head).ok()?;
    let r = contents.trim();
    // "ref: refs/heads/main" -> "main"; a bare SHA means detached HEAD.
    r.strip_prefix("ref: refs/heads/").map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A worktree must resolve back to the repo it was cut from — this is what
    /// lets agents in worktrees group under their parent project, including
    /// worktrees created by hand rather than by the CLI.
    #[test]
    fn worktree_resolves_to_its_parent_repo() {
        let tmp = tempfile::tempdir().unwrap();
        // macOS /var is a symlink to /private/var; compare canonical paths.
        let root = tmp.path().canonicalize().unwrap();
        let repo = root.join("repo");
        fs::create_dir(&repo).unwrap();

        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "t"]);
        fs::write(repo.join("f"), "x").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "init"]);

        let wt = root.join("wt");
        git(
            &repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt.to_str().unwrap(),
            ],
        );

        assert_eq!(
            main_repo_root(&repo).unwrap(),
            repo,
            "plain repo resolves to itself"
        );
        assert_eq!(
            main_repo_root(&wt).unwrap(),
            repo,
            "worktree resolves to parent"
        );
        // A subdirectory of the worktree must resolve the same way.
        let sub = wt.join("sub");
        fs::create_dir(&sub).unwrap();
        assert_eq!(main_repo_root(&sub).unwrap(), repo);

        assert_eq!(branch(&repo).as_deref(), Some("main"));
        assert_eq!(
            branch(&wt).as_deref(),
            Some("feature"),
            "worktree reports its own branch"
        );
    }

    /// git records a worktree's gitdir fully resolved. If resolution handed back
    /// anything else, a project reached through a symlink would never match its
    /// own worktree's parent and the two would render as separate groups.
    #[test]
    fn resolution_returns_canonical_paths() {
        // tempdir() sits under /var on macOS, itself a symlink to /private/var,
        // so the raw path genuinely differs from the resolved one.
        let tmp = tempfile::tempdir().unwrap();
        let repo_raw = tmp.path().join("repo");
        fs::create_dir(&repo_raw).unwrap();

        git(&repo_raw, &["init", "-q", "-b", "main"]);
        git(&repo_raw, &["config", "user.email", "t@example.com"]);
        git(&repo_raw, &["config", "user.name", "t"]);
        fs::write(repo_raw.join("f"), "x").unwrap();
        git(&repo_raw, &["add", "."]);
        git(&repo_raw, &["commit", "-qm", "init"]);

        let wt_raw = tmp.path().join("wt");
        git(
            &repo_raw,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                wt_raw.to_str().unwrap(),
            ],
        );

        let got = main_repo_root(&wt_raw).expect("worktree resolves");
        assert_eq!(got, canonical(&repo_raw), "worktree resolves to its parent");
        assert_eq!(got, canonical(&got), "and the result is already canonical");
    }

    #[test]
    fn non_repo_path_has_no_root() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(main_repo_root(tmp.path()).is_none());
        assert!(branch(tmp.path()).is_none());
    }
}
