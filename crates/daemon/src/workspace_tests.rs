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
        security: None,
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
        security: None,
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

/// The whole security phase, end to end through the runtime: a session with a
/// workspace, under a contained profile, must launch the agent *inside a
/// container* with the workspace mounted — not on the host.
#[test]
fn a_contained_session_launches_the_agent_inside_a_container() {
    let repo = repository("daemon-contained");
    let root = temp_dir("daemon-contained-root");
    std::fs::create_dir_all(&root).unwrap();

    let backend = std::sync::Arc::new(crate::backend::FakeBackend::new());
    let storage = std::sync::Arc::new(std::sync::Mutex::new(
        crate::storage::Storage::open_in_memory().unwrap(),
    ));
    let mut runtime = Runtime::new(storage, backend.clone());
    runtime.set_workspace_root(root.clone());
    runtime.set_security(security_config());

    let Request::CreateSession(mut request) = create_with_workspace("contained", &repo) else {
        panic!("expected a create request")
    };
    request.security = Some("locked".to_owned());

    let response = runtime.handle(Request::CreateSession(request));
    let Response::Session(session) = response else {
        panic!("expected created session: {response:?}")
    };
    assert!(session.security.contained);

    let launch = &backend.launches()[0].launch;
    assert_eq!(
        launch.program, "container",
        "the agent must run in a container"
    );
    assert!(launch.args.contains(&"run".to_owned()));
    assert!(launch.args.contains(&"--rm".to_owned()));
    assert!(launch.args.contains(&"anclave/agent:latest".to_owned()));

    // The workspace is mounted, and at the fixed in-container path rather
    // than wherever it happens to sit on this machine.
    let mounted = launch
        .args
        .iter()
        .any(|arg| arg.ends_with(":/workspace") && arg.starts_with(root.to_str().unwrap()));
    assert!(mounted, "workspace not mounted: {:?}", launch.args);

    // The environment travelled as -e flags, so it must not also be applied
    // by the backend's `env -i` wrapper.
    assert!(launch.environment.is_none());
    assert!(!launch.args.contains(&"--ssh".to_owned()));

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&root);
}

/// The pluggability claim, checked: the same session shape under a different
/// runtime produces a different command — and podman honours the network
/// policy that Apple's runtime has to refuse.
#[test]
fn a_podman_profile_removes_the_network() {
    let repo = repository("daemon-podman");
    let root = temp_dir("daemon-podman-root");
    std::fs::create_dir_all(&root).unwrap();

    let backend = std::sync::Arc::new(crate::backend::FakeBackend::new());
    let storage = std::sync::Arc::new(std::sync::Mutex::new(
        crate::storage::Storage::open_in_memory().unwrap(),
    ));
    let mut runtime = Runtime::new(storage, backend.clone());
    runtime.set_workspace_root(root.clone());
    runtime.set_security(security_config());

    let Request::CreateSession(mut request) = create_with_workspace("airgapped", &repo) else {
        panic!("expected a create request")
    };
    request.security = Some("airgapped".to_owned());

    let Response::Session(session) = runtime.handle(Request::CreateSession(request)) else {
        panic!("expected created session")
    };
    assert!(session.security.contained);

    let launch = &backend.launches()[0].launch;
    assert_eq!(launch.program, "podman");
    let network_at = launch
        .args
        .iter()
        .position(|a| a == "--network")
        .expect("podman must remove the network");
    assert_eq!(launch.args[network_at + 1], "none");
    assert!(launch.args.contains(&"--cap-drop=ALL".to_owned()));

    let _ = std::fs::remove_dir_all(&repo);
    let _ = std::fs::remove_dir_all(&root);
}

/// Two contained profiles that name their runtime explicitly, so these tests
/// assert Anclave's behaviour rather than what happens to be installed.
fn security_config() -> anclave_security::SecurityConfig {
    anclave_security::SecurityConfig::parse(
        "default = \"host\"\n\n\
         [profiles.host]\nsandbox = \"host\"\n\n\
         [profiles.locked]\nsandbox = \"container\"\nimage = \"anclave/agent:latest\"\n\
         runtime = \"apple-container\"\n\
         credentials = { mode = \"none\" }\nfilesystem = \"workspace\"\n\n\
         [profiles.airgapped]\nsandbox = \"container\"\nimage = \"anclave/agent:latest\"\n\
         runtime = \"podman\"\n\
         credentials = { mode = \"none\" }\nfilesystem = \"workspace\"\n\
         network = { mode = \"none\" }\n",
    )
    .expect("the test security config is valid")
}
