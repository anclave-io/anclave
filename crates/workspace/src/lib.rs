use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInfo {
    pub root: PathBuf,
    pub branch: Option<String>,
    pub remote_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
}

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("repository path does not exist: {0}")]
    MissingPath(PathBuf),
    #[error("path is not a Git repository: {0}")]
    NotRepository(PathBuf),
    #[error("worktree path already exists: {0}")]
    WorktreeExists(PathBuf),
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

pub fn create_worktree(
    repository: impl AsRef<Path>,
    path: impl AsRef<Path>,
    branch: &str,
    base: Option<&str>,
) -> Result<Worktree, RepositoryError> {
    let repository = inspect(repository)?.root;
    let path = path.as_ref().to_path_buf();
    validate_branch(branch)?;
    if path.exists() {
        return Err(RepositoryError::WorktreeExists(path));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            RepositoryError::GitFailed(format!("create worktree parent: {error}"))
        })?;
    }
    let mut args = vec!["worktree", "add", "-b", branch];
    let path_string = path.to_string_lossy().into_owned();
    args.push(&path_string);
    if let Some(base) = base {
        validate_revision(base)?;
        args.push(base);
    }
    let result = run_git_args(&repository, args);
    if let Err(error) = result {
        let _ = std::fs::remove_dir(&path);
        return Err(error);
    }
    Ok(Worktree {
        path,
        branch: branch.to_owned(),
    })
}

pub fn remove_worktree(
    repository: impl AsRef<Path>,
    worktree: impl AsRef<Path>,
) -> Result<(), RepositoryError> {
    let repository = inspect(repository)?.root;
    let worktree = worktree.as_ref();
    if !worktree.exists() {
        return Ok(());
    }
    run_git_args(
        &repository,
        [
            "worktree",
            "remove",
            "--force",
            worktree.to_string_lossy().as_ref(),
        ],
    )?;
    Ok(())
}

fn validate_branch(branch: &str) -> Result<(), RepositoryError> {
    if branch.is_empty()
        || branch.starts_with('-')
        || branch.contains("..")
        || branch.contains('\0')
    {
        return Err(RepositoryError::GitFailed("invalid branch name".to_owned()));
    }
    Ok(())
}

fn validate_revision(revision: &str) -> Result<(), RepositoryError> {
    if revision.is_empty() || revision.starts_with('-') || revision.contains('\0') {
        return Err(RepositoryError::GitFailed(
            "invalid base revision".to_owned(),
        ));
    }
    Ok(())
}

fn run_git<I, S>(directory: &Path, args: I) -> Result<String, RepositoryError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new("git");
    command.current_dir(directory).args(args);
    scrub_git_environment(&mut command);
    let output = command
        .output()
        .map_err(|error| RepositoryError::GitUnavailable(error.to_string()))?;
    checked_output(output)
}

fn run_git_args<I, S>(directory: &Path, args: I) -> Result<String, RepositoryError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<std::ffi::OsStr>,
{
    run_git(directory, args)
}

fn scrub_git_environment(command: &mut Command) {
    for variable in GIT_LOCATION_VARS {
        command.env_remove(variable);
    }
    command.env_remove("GIT_CONFIG_PARAMETERS");
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
        scrub_git_environment(&mut command);
        assert!(
            command.status().unwrap().success(),
            "git command failed: {args:?}"
        );
    }
    fn repository(label: &str) -> PathBuf {
        let path = temp_dir(label);
        fs::create_dir_all(&path).unwrap();
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
        path
    }

    #[test]
    fn inspects_branch_root_and_origin() {
        let path = repository("repo");
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
    fn worktree_lifecycle_is_explicit_and_repeatable() {
        let repo = repository("worktree");
        let path = repo.parent().unwrap().join("checkout");
        let worktree = create_worktree(&repo, &path, "feature/worktree", None).unwrap();
        assert_eq!(worktree.branch, "feature/worktree");
        assert!(path.join("README").exists());
        assert!(matches!(
            create_worktree(&repo, &path, "other", None),
            Err(RepositoryError::WorktreeExists(_))
        ));
        remove_worktree(&repo, &path).unwrap();
        assert!(!path.exists());
        remove_worktree(&repo, &path).unwrap();
        let _ = fs::remove_dir_all(repo);
    }
    #[test]
    fn git_location_environment_does_not_override_repository_detection() {
        let path = repository("scrub");
        let previous = std::env::var_os("GIT_DIR");
        std::env::set_var("GIT_DIR", "/definitely/not-this-repository");
        assert!(is_repository(&path));
        match previous {
            Some(value) => std::env::set_var("GIT_DIR", value),
            None => std::env::remove_var("GIT_DIR"),
        };
        let _ = fs::remove_dir_all(path);
    }
}
