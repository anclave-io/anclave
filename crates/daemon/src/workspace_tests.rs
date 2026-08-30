use std::sync::{Arc, Mutex};

use anclave_protocol::{
    AgentId, BackendId, CreateSession, Request, Response, SessionState, WorkspaceId, WorkspaceSpec,
};

use crate::backend::FakeBackend;
use crate::runtime::Runtime;
use crate::storage::Storage;

fn temp_dir(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "anclave-daemon-ws-{}-{}",
        label,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn repository(label: &str) -> std::path::PathBuf {
    let path = temp_dir(label);
    std::fs::create_dir_all(&path).unwrap();
    std::process::Command::new("git")
        .arg("init")
        .arg("-q")
        .current_dir(&path)
        .status()
        .unwrap();
    std::fs::write(path.join("README"), "test").unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.email=t@test",
            "-c",
            "user.name=T",
            "add",
            "README",
        ])
        .current_dir(&path)
        .status()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.email=t@test",
            "-c",
            "user.name=T",
            "commit",
            "-qm",
            "init",
        ])
        .current_dir(&path)
        .status()
        .unwrap();
    path
}

fn workspace_runtime(workspace_root: std::path::PathBuf) -> Runtime {
    let mut runtime = Runtime::new(
        Arc::new(Mutex::new(Storage::open_in_memory().unwrap())),
        Arc::new(FakeBackend::new()),
    );
    runtime.set_workspace_root(workspace_root);
    runtime
}

fn create_with_workspace(name: &str, repo: &std::path::Path) -> Request {
    Request::CreateSession(CreateSession {
        name: name.to_owned(),
        agent: AgentId::new("default").unwrap(),
        backend: BackendId::new("local").unwrap(),
        workspace: Some(WorkspaceSpec::single(
            WorkspaceId::new("ws-test").unwrap(),
            repo.to_string_lossy().into_owned(),
            "feature/test",
        )),
    })
}

#[test]
fn session_create_with_workspace_creates_worktree() {
    let repo = repository("daemon-ws-create");
    let root = temp_dir("daemon-ws-root");
    std::fs::create_dir_all(&root).unwrap();

    let runtime = workspace_runtime(root.clone());
    let response = runtime.handle(create_with_workspace("demo", &repo));
    let Response::Session(session) = response else {
        panic!("expected created session: {response:?}")
    };
    assert_eq!(session.state, SessionState::Running);
    // A single-member workspace nests the checkout under the workspace
    // directory and runs the agent inside it, so the repository keeps its own
    // name in the path the agent sees.
    let workspace = root.join("ws-test");
    let checkout = workspace.join(repo.file_name().unwrap());
    assert!(checkout.join("README").exists());

    let delete_response = runtime.handle(Request::DeleteSession {
        id: session.id.clone(),
    });
    assert!(
        matches!(delete_response, Response::Accepted),
        "expected Accepted, got: {delete_response:?}"
    );
    assert!(!workspace.exists());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn session_create_without_workspace_skips_worktree() {
    let root = temp_dir("daemon-ws-noop");
    std::fs::create_dir_all(&root).unwrap();

    let runtime = workspace_runtime(root.clone());
    let request = Request::CreateSession(CreateSession {
        name: "demo".to_owned(),
        agent: AgentId::new("default").unwrap(),
        backend: BackendId::new("local").unwrap(),
        workspace: None,
    });
    let Response::Session(session) = runtime.handle(request) else {
        panic!("expected created session")
    };
    assert!(session.workspace.is_none());
    assert_eq!(root.read_dir().unwrap().count(), 0);

    let delete_response = runtime.handle(Request::DeleteSession { id: session.id });
    assert!(
        matches!(delete_response, Response::Accepted),
        "expected Accepted, got: {delete_response:?}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn delete_cleans_up_workspace_for_session() {
    let repo = repository("daemon-ws-delete");
    let root = temp_dir("daemon-ws-delete-root");
    std::fs::create_dir_all(&root).unwrap();

    let runtime = workspace_runtime(root.clone());
    let Response::Session(session) = runtime.handle(create_with_workspace("demo", &repo)) else {
        panic!("expected created session")
    };
    assert!(root.join("ws-test").exists());

    let delete_response = runtime.handle(Request::DeleteSession {
        id: session.id.clone(),
    });
    assert!(
        matches!(delete_response, Response::Accepted),
        "expected Accepted, got: {delete_response:?}"
    );
    assert!(!root.join("ws-test").exists());
    std::fs::remove_dir_all(&root).unwrap();
}
