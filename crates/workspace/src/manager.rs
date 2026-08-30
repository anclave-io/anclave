//! Per-session workspaces: one directory gathering every repository a session
//! works across.
//!
//! A member with a branch gets its own Git worktree inside the workspace; a
//! member without one is symlinked in as it is. With a single member the agent
//! runs directly in that member, so the common one-repository case has no
//! wrapper directory. With several, the agent runs in the workspace root and
//! sees each repository as a subdirectory — which needs no per-agent
//! `--add-dir` flag and so works with any CLI.
//!
//! **A workspace is not a sandbox.** It arranges directories. It does not
//! constrain the process that runs in them.

use anclave_protocol::{WorkspaceMember, WorkspaceSpec};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct WorkspaceManager {
    root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("workspace error: {0}")]
    Git(#[from] crate::RepositoryError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("workspace has no members")]
    Empty,
    #[error("member name '{0}' would escape the workspace directory")]
    UnsafeMemberName(String),
}

/// The directory name a member takes inside the workspace.
///
/// Derived from the repository's last path component, because that is the name
/// a person recognises. Two members can legitimately share one — `web/api` and
/// `mobile/api` — so a collision is disambiguated by suffix in member order
/// rather than rejected: refusing would make an ordinary pair of repositories
/// unusable together.
fn member_name(repository: &str) -> Result<String, WorkspaceError> {
    let raw = Path::new(repository)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    // `..`, an empty name, or any separator would place the member outside the
    // workspace directory. A repository path ending in `..` is the realistic
    // way this happens, not an attack.
    if raw.is_empty()
        || raw == "."
        || raw == ".."
        || raw.contains('/')
        || raw.contains('\\')
        || raw.contains('\0')
    {
        return Err(WorkspaceError::UnsafeMemberName(raw));
    }
    Ok(raw)
}

/// Assign each member its directory name, disambiguating collisions.
pub fn member_names(members: &[WorkspaceMember]) -> Result<Vec<String>, WorkspaceError> {
    let mut used: HashMap<String, usize> = HashMap::new();
    let mut names = Vec::with_capacity(members.len());
    for member in members {
        let base = member_name(&member.repository)?;
        let count = used.entry(base.clone()).or_insert(0);
        *count += 1;
        names.push(if *count == 1 {
            base
        } else {
            format!("{base}-{count}")
        });
    }
    Ok(names)
}

impl WorkspaceManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The workspace's own directory, whether or not it exists.
    pub fn workspace_path(&self, spec: &WorkspaceSpec) -> PathBuf {
        self.root.join(spec.id.as_str())
    }

    /// Build the workspace and return the directory the agent should run in.
    ///
    /// Creation is all-or-nothing: a member that cannot be prepared removes
    /// everything already built, so a failed create never leaves a partial
    /// workspace for the next attempt to trip over.
    pub fn create(&self, spec: &WorkspaceSpec) -> Result<PathBuf, WorkspaceError> {
        if spec.members.is_empty() {
            return Err(WorkspaceError::Empty);
        }
        let names = member_names(&spec.members)?;
        let workspace = self.workspace_path(spec);
        std::fs::create_dir_all(&workspace)?;

        for (member, name) in spec.members.iter().zip(&names) {
            if let Err(error) = self.prepare_member(&workspace, member, name) {
                self.cleanup(spec);
                return Err(error);
            }
        }

        // One member means no wrapper: the agent runs in the repository, which
        // is what a single-repo session expects and what a cwd-scoped agent's
        // "resume the last session here" depends on.
        Ok(if spec.members.len() == 1 {
            workspace.join(&names[0])
        } else {
            workspace
        })
    }

    fn prepare_member(
        &self,
        workspace: &Path,
        member: &WorkspaceMember,
        name: &str,
    ) -> Result<(), WorkspaceError> {
        let destination = workspace.join(name);

        match &member.branch {
            // A worktree needs a repository to branch from.
            Some(branch) => {
                let repository = crate::inspect(&member.repository)?.root;
                crate::create_worktree(&repository, &destination, branch, member.base.as_deref())?;
            }
            // An attached member does not: a directory of reference material
            // or notes is a legitimate thing to put in front of an agent, and
            // requiring `git init` on it would be arbitrary. It only has to
            // exist.
            None => {
                let source = std::fs::canonicalize(&member.repository).map_err(|error| {
                    crate::RepositoryError::GitFailed(format!(
                        "workspace member {}: {error}",
                        member.repository
                    ))
                })?;
                symlink_member(&source, &destination)?;
            }
        }
        Ok(())
    }

    /// Remove the workspace and every worktree it owns.
    ///
    /// Best-effort and idempotent: cleanup runs on paths that may be partly
    /// built or already gone, and a failure here must not mask the error that
    /// caused it.
    pub fn cleanup(&self, spec: &WorkspaceSpec) {
        let workspace = self.workspace_path(spec);
        if !workspace.exists() {
            return;
        }
        if let Ok(names) = member_names(&spec.members) {
            for (member, name) in spec.members.iter().zip(&names) {
                if member.branch.is_some() {
                    let _ = crate::remove_worktree(&member.repository, workspace.join(name));
                }
            }
        }
        let _ = std::fs::remove_dir_all(&workspace);
    }

    pub fn cleanup_path(&self, path: &Path) {
        if path.exists() {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    pub fn adopt_if_exists(&self, spec: &WorkspaceSpec) -> bool {
        self.workspace_path(spec).exists()
    }
}

/// Attach a member that keeps its own checkout.
///
/// A symlink rather than a copy: the point is that the agent sees the *same*
/// working tree the user does.
fn symlink_member(repository: &Path, destination: &Path) -> Result<(), WorkspaceError> {
    #[cfg(unix)]
    std::os::unix::fs::symlink(repository, destination)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(repository, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anclave_protocol::{MemberAccess, WorkspaceId};
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "anclave-wm-{}-{}",
            label,
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn git(path: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        command.current_dir(path).args(args);
        for var in &["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            command.env_remove(var);
        }
        assert!(command.status().unwrap().success(), "git failed: {args:?}");
    }

    fn repository(label: &str) -> PathBuf {
        let path = temp_dir(label);
        std::fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q"]);
        std::fs::write(path.join("README"), "test").unwrap();
        git(
            &path,
            &[
                "-c",
                "user.email=t@test",
                "-c",
                "user.name=T",
                "add",
                "README",
            ],
        );
        git(
            &path,
            &[
                "-c",
                "user.email=t@test",
                "-c",
                "user.name=T",
                "commit",
                "-qm",
                "init",
            ],
        );
        path
    }

    fn member(repository: &Path, branch: Option<&str>) -> WorkspaceMember {
        WorkspaceMember {
            repository: repository.to_string_lossy().into_owned(),
            branch: branch.map(str::to_owned),
            base: None,
            access: MemberAccess::ReadWrite,
        }
    }

    fn spec(id: &str, members: Vec<WorkspaceMember>) -> WorkspaceSpec {
        WorkspaceSpec {
            id: WorkspaceId::new(id).unwrap(),
            members,
        }
    }

    #[test]
    fn a_single_member_launches_in_the_repository_itself() {
        let repo = repository("single");
        let root = temp_dir("single-root");
        let manager = WorkspaceManager::new(&root);
        let spec = spec("ws-1", vec![member(&repo, Some("feature/single"))]);

        let cwd = manager.create(&spec).unwrap();
        // No wrapper directory: the agent runs in the checkout.
        assert_eq!(cwd, root.join("ws-1").join(repo.file_name().unwrap()));
        assert!(cwd.join("README").exists());

        manager.cleanup(&spec);
        assert!(!manager.workspace_path(&spec).exists());
        let _ = std::fs::remove_dir_all(repo);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn several_members_are_gathered_under_one_directory() {
        let first = repository("multi-a");
        let second = repository("multi-b");
        let root = temp_dir("multi-root");
        let manager = WorkspaceManager::new(&root);
        let spec = spec(
            "ws-2",
            vec![
                member(&first, Some("feature/multi")),
                member(&second, Some("feature/multi")),
            ],
        );

        let cwd = manager.create(&spec).unwrap();
        assert_eq!(cwd, root.join("ws-2"));
        assert!(cwd.join(first.file_name().unwrap()).join("README").exists());
        assert!(cwd
            .join(second.file_name().unwrap())
            .join("README")
            .exists());

        manager.cleanup(&spec);
        assert!(!cwd.exists());
        for path in [first, second, root] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn a_member_without_a_branch_is_attached_as_it_is() {
        let worktreed = repository("asis-a");
        let attached = repository("asis-b");
        let root = temp_dir("asis-root");
        let manager = WorkspaceManager::new(&root);
        let spec = spec(
            "ws-3",
            vec![
                member(&worktreed, Some("feature/asis")),
                member(&attached, None),
            ],
        );

        let cwd = manager.create(&spec).unwrap();
        let link = cwd.join(attached.file_name().unwrap());
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        // The symlink resolves to the user's own checkout, which is the point.
        assert_eq!(
            std::fs::canonicalize(&link).unwrap(),
            std::fs::canonicalize(&attached).unwrap()
        );

        manager.cleanup(&spec);
        // Cleanup must not follow the link and delete the real repository.
        assert!(attached.join("README").exists());
        for path in [worktreed, attached, root] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    /// `web/api` and `mobile/api` is an ordinary pair, not an error.
    /// Reference material is not a repository, and requiring `git init` on a
    /// docs directory would be arbitrary.
    #[test]
    fn an_attached_member_need_not_be_a_repository() {
        let repo = repository("plain-a");
        let plain = temp_dir("plain-dir");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("NOTES"), "reference").unwrap();
        let root = temp_dir("plain-root");
        let manager = WorkspaceManager::new(&root);
        let spec = spec(
            "ws-plain",
            vec![member(&repo, Some("feature/plain")), member(&plain, None)],
        );

        let cwd = manager.create(&spec).unwrap();
        assert!(cwd.join(plain.file_name().unwrap()).join("NOTES").exists());

        manager.cleanup(&spec);
        assert!(plain.join("NOTES").exists(), "the real directory survives");
        for path in [repo, plain, root] {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[test]
    fn a_member_directory_that_does_not_exist_is_refused() {
        let root = temp_dir("absent-root");
        let manager = WorkspaceManager::new(&root);
        let spec = spec(
            "ws-absent",
            vec![WorkspaceMember {
                repository: "/definitely/not/here".to_owned(),
                branch: None,
                base: None,
                access: MemberAccess::ReadWrite,
            }],
        );
        assert!(manager.create(&spec).is_err());
        assert!(!manager.workspace_path(&spec).exists());
    }

    #[test]
    fn duplicate_basenames_are_disambiguated_in_order() {
        let members = vec![
            WorkspaceMember {
                repository: "/src/web/api".to_owned(),
                branch: None,
                base: None,
                access: MemberAccess::ReadWrite,
            },
            WorkspaceMember {
                repository: "/src/mobile/api".to_owned(),
                branch: None,
                base: None,
                access: MemberAccess::ReadWrite,
            },
            WorkspaceMember {
                repository: "/src/desktop/api".to_owned(),
                branch: None,
                base: None,
                access: MemberAccess::ReadWrite,
            },
        ];
        assert_eq!(member_names(&members).unwrap(), ["api", "api-2", "api-3"]);
    }

    #[test]
    fn a_name_that_would_escape_the_workspace_is_rejected() {
        for repository in ["/src/..", "/", "..", ""] {
            let members = vec![WorkspaceMember {
                repository: repository.to_owned(),
                branch: None,
                base: None,
                access: MemberAccess::ReadWrite,
            }];
            assert!(
                matches!(
                    member_names(&members),
                    Err(WorkspaceError::UnsafeMemberName(_))
                ),
                "{repository} should be refused"
            );
        }

        // `.` is not traversal: Rust normalises it away, so `/src/.` is
        // simply `/src` and takes the name `src`.
        let dot = vec![WorkspaceMember {
            repository: "/src/.".to_owned(),
            branch: None,
            base: None,
            access: MemberAccess::ReadWrite,
        }];
        assert_eq!(member_names(&dot).unwrap(), ["src"]);
    }

    /// One bad member must not leave a half-built workspace behind, or the
    /// next attempt trips over the leftovers.
    #[test]
    fn a_missing_member_rolls_the_whole_workspace_back() {
        let good = repository("rollback");
        let root = temp_dir("rollback-root");
        let manager = WorkspaceManager::new(&root);
        let missing = WorkspaceMember {
            repository: "/definitely/not/a/repository".to_owned(),
            branch: Some("feature/x".to_owned()),
            base: None,
            access: MemberAccess::ReadWrite,
        };
        let spec = spec("ws-4", vec![member(&good, Some("feature/x")), missing]);

        assert!(manager.create(&spec).is_err());
        assert!(
            !manager.workspace_path(&spec).exists(),
            "a failed create must leave nothing behind"
        );

        // And the rollback must have released the branch, so a retry works.
        let retry = spec_retry(&good);
        let manager2 = WorkspaceManager::new(&root);
        assert!(manager2.create(&retry).is_ok());

        manager2.cleanup(&retry);
        let _ = std::fs::remove_dir_all(good);
        let _ = std::fs::remove_dir_all(root);
    }

    fn spec_retry(repo: &Path) -> WorkspaceSpec {
        WorkspaceSpec {
            id: WorkspaceId::new("ws-5").unwrap(),
            members: vec![WorkspaceMember {
                repository: repo.to_string_lossy().into_owned(),
                branch: Some("feature/retry".to_owned()),
                base: None,
                access: MemberAccess::ReadWrite,
            }],
        }
    }

    #[test]
    fn an_empty_workspace_is_refused() {
        let manager = WorkspaceManager::new(temp_dir("empty-root"));
        let spec = spec("ws-6", Vec::new());
        assert!(matches!(manager.create(&spec), Err(WorkspaceError::Empty)));
    }

    #[test]
    fn cleanup_is_idempotent() {
        let root = temp_dir("idem-root");
        let manager = WorkspaceManager::new(&root);
        let spec = spec("ws-7", vec![]);
        manager.cleanup(&spec);
        manager.cleanup(&spec);
        assert!(!manager.workspace_path(&spec).exists());
    }
}
