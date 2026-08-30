use anclave_protocol::{SessionId, WorkspaceSpec};
use std::path::{Path, PathBuf};
use std::process::Command;

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
}

impl WorkspaceManager {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn worktree_path(&self, spec: &WorkspaceSpec) -> PathBuf {
        self.root.join(spec.id.as_str())
    }

    pub fn create(&self, spec: &WorkspaceSpec) -> Result<PathBuf, WorkspaceError> {
        let repository = crate::inspect(&spec.repository)?.root;
        let path = self.root.join(spec.id.as_str());
        crate::create_worktree(&repository, &path, &spec.branch, spec.base.as_deref())?;
        Ok(path)
    }

    pub fn cleanup(&self, spec: &WorkspaceSpec) {
        let path = self.worktree_path(spec);
        if path.exists() {
            let _ = crate::remove_worktree(&spec.repository, &path);
            let _ = std::fs::remove_dir_all(&path);
        }
    }

    pub fn cleanup_path(&self, path: &Path) {
        if path.exists() {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    pub fn adopt_if_exists(&self, spec: &WorkspaceSpec) -> bool {
        self.worktree_path(spec).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anclave_protocol::WorkspaceId;
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

    #[test]
    fn creates_and_cleans_up_worktree() {
        let repo = repository("wm-create");
        let root = temp_dir("wm-root");
        std::fs::create_dir_all(&root).unwrap();

        let wm = WorkspaceManager::new(&root);
        let spec = WorkspaceSpec {
            id: WorkspaceId::new("wt-1").unwrap(),
            repository: repo.to_string_lossy().into_owned(),
            branch: "feature/wm".to_owned(),
            base: None,
        };

        let path = wm.create(&spec).unwrap();
        assert_eq!(path, root.join("wt-1"));
        assert!(path.join("README").exists());
        assert!(wm.adopt_if_exists(&spec));

        wm.cleanup(&spec);
        assert!(!path.exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn cleanup_is_idempotent() {
        let root = temp_dir("wm-idem");
        std::fs::create_dir_all(&root).unwrap();
        let wm = WorkspaceManager::new(&root);
        let spec = WorkspaceSpec {
            id: WorkspaceId::new("wt-2").unwrap(),
            repository: "/nonexistent".to_owned(),
            branch: "main".to_owned(),
            base: None,
        };
        wm.cleanup(&spec);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn worktree_path_derived_from_id() {
        let wm = WorkspaceManager::new("/tmp/test-root");
        let spec = WorkspaceSpec {
            id: WorkspaceId::new("session-42").unwrap(),
            repository: "/repo".to_owned(),
            branch: "main".to_owned(),
            base: None,
        };
        assert_eq!(
            wm.worktree_path(&spec),
            PathBuf::from("/tmp/test-root/session-42")
        );
    }
}
