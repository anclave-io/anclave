use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInfo {
    pub root: PathBuf,
    pub branch: Option<String>,
    pub remote_url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("repository path does not exist: {0}")]
    MissingPath(PathBuf),
    #[error("path is not a Git repository: {0}")]
    NotRepository(PathBuf),
    #[error("Git is unavailable: {0}")]
    GitUnavailable(String),
    #[error("Git command failed: {0}")]
    GitFailed(String),
    #[error("Git returned invalid UTF-8")]
    InvalidOutput,
}

const GIT_LOCATION_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
];

pub fn inspect(path: impl AsRef<Path>) -> Result<RepositoryInfo, RepositoryError> {
    let path = path.as_ref();
    if !path.exists() {
        return Err(RepositoryError::MissingPath(path.to_path_buf()));
    }
    let root = run_git(path, ["rev-parse", "--show-toplevel"]).map_err(|error| match error {
        RepositoryError::GitFailed(_) => RepositoryError::NotRepository(path.to_path_buf()),
        other => other,
    })?;
    let root = PathBuf::from(root.trim());
    let branch = run_git(&root, ["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    let remote_url = run_git(&root, ["config", "--get", "remote.origin.url"])
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    Ok(RepositoryInfo {
        root,
        branch,
        remote_url,
    })
}

pub fn is_repository(path: impl AsRef<Path>) -> bool {
    inspect(path).is_ok()
}

fn run_git<I, S>(directory: &Path, args: I) -> Result<String, RepositoryError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    command.current_dir(directory).args(args);
    for variable in GIT_LOCATION_VARS {
        command.env_remove(variable);
    }
    command.env_remove("GIT_CONFIG_PARAMETERS");
    let output = command
        .output()
        .map_err(|error| RepositoryError::GitUnavailable(error.to_string()))?;
    checked_output(output)
}

fn checked_output(output: Output) -> Result<String, RepositoryError> {
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(RepositoryError::GitFailed(if message.is_empty() {
            "unknown Git error".to_owned()
        } else {
            message
        }));
    }
    String::from_utf8(output.stdout).map_err(|_| RepositoryError::InvalidOutput)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "anclave-workspace-{label}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn git(path: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        command.current_dir(path).args(args);
        for variable in GIT_LOCATION_VARS {
            command.env_remove(variable);
        }
        command.env_remove("GIT_CONFIG_PARAMETERS");
        let status = command.status().unwrap();
        assert!(status.success(), "git command failed: {args:?}");
    }

    #[test]
    fn inspects_branch_root_and_origin() {
        let path = temp_dir("repo");
        fs::create_dir_all(&path).unwrap();
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::remove_var("GIT_DIR");
        git(&path, &["init", "-q"]);
        fs::write(path.join("README"), "test").unwrap();
        git(
            &path,
            &[
                "-c",
                "user.email=anclave@example.test",
                "-c",
                "user.name=Anclave Test",
                "add",
                "README",
            ],
        );
        git(
            &path,
            &[
                "-c",
                "user.email=anclave@example.test",
                "-c",
                "user.name=Anclave Test",
                "commit",
                "-qm",
                "initial",
            ],
        );
        git(&path, &["checkout", "-qb", "feature/test"]);
        git(
            &path,
            &["remote", "add", "origin", "https://example.test/repo.git"],
        );
        let info = inspect(&path).unwrap();
        assert_eq!(info.root, fs::canonicalize(&path).unwrap());
        assert_eq!(info.branch.as_deref(), Some("feature/test"));
        assert_eq!(
            info.remote_url.as_deref(),
            Some("https://example.test/repo.git")
        );
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn rejects_missing_and_non_repository_paths() {
        let missing = temp_dir("missing");
        assert!(matches!(
            inspect(&missing),
            Err(RepositoryError::MissingPath(_))
        ));
        let path = temp_dir("not-repo");
        fs::create_dir_all(&path).unwrap();
        assert!(matches!(
            inspect(&path),
            Err(RepositoryError::NotRepository(_))
        ));
        let _ = fs::remove_dir_all(path);
    }

    #[test]
    fn git_location_environment_does_not_override_repository_detection() {
        let path = temp_dir("scrub");
        fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q"]);
        let previous = std::env::var_os("GIT_DIR");
        std::env::remove_var("GIT_CONFIG_NOSYSTEM");
        std::env::set_var("GIT_DIR", "/definitely/not-this-repository");
        assert!(is_repository(&path));
        match previous {
            Some(value) => std::env::set_var("GIT_DIR", value),
            None => std::env::remove_var("GIT_DIR"),
        }
        let _ = fs::remove_dir_all(path);
    }
}
