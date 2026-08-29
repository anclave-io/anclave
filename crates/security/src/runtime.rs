//! Which sandbox runtimes exist on this machine, and which one to use.
//!
//! Anclave does not hard-code a containment technology. Each platform has a
//! different best answer, the answers move, and a machine may have none of
//! them — so the daemon *probes* rather than assumes, and reports what it
//! found instead of failing obscurely at the first launch.
//!
//! The ranking below is deliberately about **isolation strength first**, not
//! convenience. A process-isolated Windows container and a Hyper-V-isolated
//! one are both "a container" and are not remotely the same boundary; a
//! catalogue that ranked by ease of setup would put the weaker one first.

use std::process::Command;

/// How strong a boundary a runtime actually provides.
///
/// The distinction that matters: sharing the host kernel means a kernel bug
/// is a full escape. That is an acceptable trade for many teams and an
/// unacceptable one for others, and neither can decide without being told
/// which they are getting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Isolation {
    /// No boundary. The agent runs as you.
    None,
    /// OS-level: namespaces, cgroups, a restricted token. Real, but the
    /// kernel is shared with the host.
    Kernel,
    /// A separate kernel in a virtual machine. The strongest option
    /// generally available, and the slowest to start.
    Machine,
}

impl Isolation {
    pub fn describe(self) -> &'static str {
        match self {
            Self::None => "no containment",
            Self::Kernel => "OS-level, shares the host kernel",
            Self::Machine => "separate kernel in a VM",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    MacOs,
    Linux,
    Windows,
    Other,
}

impl Platform {
    /// The platform this binary was built for.
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Other
        }
    }
}

/// A containment technology Anclave knows how to look for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runtime {
    /// Apple's containerization framework: each container in its own
    /// lightweight VM on Apple silicon.
    AppleContainer,
    /// macOS Seatbelt. Native and genuinely enforcing, but Apple has marked
    /// the interface deprecated, so it is a fallback rather than a plan.
    SandboxExec,
    /// Rootless Linux containers. The pragmatic Linux default.
    Podman,
    /// Linux containers via Docker.
    Docker,
    /// MicroVMs. The strongest Linux option and the most work to operate.
    Firecracker,
    /// Unprivileged namespace sandboxing, no daemon.
    Bubblewrap,
    /// Hyper-V-isolated Windows containers.
    HypervContainer,
    /// The disposable desktop VM. Real isolation, awkward to drive per
    /// session.
    WindowsSandbox,
    /// A Linux VM hosting one of the Linux runtimes above.
    Wsl2,
}

impl Runtime {
    pub fn name(self) -> &'static str {
        match self {
            Self::AppleContainer => "apple-container",
            Self::SandboxExec => "sandbox-exec",
            Self::Podman => "podman",
            Self::Docker => "docker",
            Self::Firecracker => "firecracker",
            Self::Bubblewrap => "bubblewrap",
            Self::HypervContainer => "hyperv-container",
            Self::WindowsSandbox => "windows-sandbox",
            Self::Wsl2 => "wsl2",
        }
    }

    pub fn isolation(self) -> Isolation {
        match self {
            Self::AppleContainer
            | Self::Firecracker
            | Self::HypervContainer
            | Self::WindowsSandbox
            | Self::Wsl2 => Isolation::Machine,
            Self::SandboxExec | Self::Podman | Self::Docker | Self::Bubblewrap => Isolation::Kernel,
        }
    }

    /// The command probed to decide whether this runtime is usable, and the
    /// argument that makes it answer cheaply.
    pub fn probe_command(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Self::AppleContainer => ("container", &["--version"]),
            Self::SandboxExec => ("sandbox-exec", &["-n", "no-internet", "/usr/bin/true"]),
            Self::Podman => ("podman", &["--version"]),
            Self::Docker => ("docker", &["version", "--format", "{{.Server.Os}}"]),
            Self::Firecracker => ("firecracker", &["--version"]),
            Self::Bubblewrap => ("bwrap", &["--version"]),
            Self::HypervContainer => ("docker", &["version", "--format", "{{.Server.Os}}"]),
            Self::WindowsSandbox => ("WindowsSandbox", &["/?"]),
            Self::Wsl2 => ("wsl.exe", &["--status"]),
        }
    }

    /// What an operator needs to know before choosing it.
    pub fn caveat(self) -> &'static str {
        match self {
            Self::AppleContainer => "Apple silicon only; needs a recent macOS",
            Self::SandboxExec => "deprecated by Apple — a fallback, not a plan",
            Self::Podman => "rootless by default; shares the host kernel",
            Self::Docker => "the daemon runs as root; shares the host kernel",
            Self::Firecracker => "strongest, but you operate the VM images yourself",
            Self::Bubblewrap => "no daemon, no images; you supply the filesystem",
            Self::HypervContainer => "Windows Pro or Enterprise, Hyper-V enabled",
            Self::WindowsSandbox => "disposable desktop VM; awkward to drive per session",
            Self::Wsl2 => "one shared VM — isolates from Windows, not between sessions",
        }
    }
}

/// The runtimes worth looking for on a platform, strongest first.
///
/// Pure: the ordering is a decision, and a decision should be testable
/// without a machine that happens to have these installed.
pub fn catalog(platform: Platform) -> Vec<Runtime> {
    match platform {
        // Apple's own containerization gives a per-container VM, which beats
        // Seatbelt's kernel-level restrictions on strength and is not
        // deprecated.
        Platform::MacOs => vec![
            Runtime::AppleContainer,
            Runtime::Podman,
            Runtime::Docker,
            Runtime::SandboxExec,
        ],
        // Firecracker is stronger; podman is what most machines actually
        // have, so it ranks below on strength and wins in practice by being
        // present.
        Platform::Linux => vec![
            Runtime::Firecracker,
            Runtime::Podman,
            Runtime::Docker,
            Runtime::Bubblewrap,
        ],
        // The least settled of the three. Hyper-V isolation is the only
        // option here that is both a real boundary and drivable per session;
        // WSL2 is listed because it is what people have, with the caveat that
        // one shared VM does not isolate sessions from each other.
        Platform::Windows => vec![
            Runtime::HypervContainer,
            Runtime::WindowsSandbox,
            Runtime::Wsl2,
        ],
        Platform::Other => Vec::new(),
    }
}

/// One runtime, and whether this machine can use it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub runtime: Runtime,
    pub available: bool,
    pub detail: String,
}

/// What the daemon found, and what it suggests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub platform: Platform,
    pub candidates: Vec<Candidate>,
    /// The strongest available runtime, if any.
    pub recommended: Option<Runtime>,
}

impl Report {
    /// Whether this machine can contain an agent at all.
    pub fn can_contain(&self) -> bool {
        self.recommended.is_some()
    }
}

/// Choose from an already-probed list.
///
/// Split from probing so the choice is testable against any machine shape,
/// including ones nobody has.
pub fn recommend(candidates: &[Candidate]) -> Option<Runtime> {
    candidates
        .iter()
        .filter(|candidate| candidate.available)
        .map(|candidate| candidate.runtime)
        // The catalogue is already strongest-first, so the max by isolation
        // with catalogue order as the tiebreak is the first available one.
        .next()
}

/// Probe this machine. Runs one short command per candidate.
pub fn detect() -> Report {
    detect_with(Platform::current(), &probe)
}

/// Probe with an injected prober, for tests and for a remote host later.
pub fn detect_with(
    platform: Platform,
    prober: &dyn Fn(Runtime) -> Result<String, String>,
) -> Report {
    let candidates: Vec<Candidate> = catalog(platform)
        .into_iter()
        .map(|runtime| match prober(runtime) {
            Ok(detail) => Candidate {
                runtime,
                available: true,
                detail,
            },
            Err(reason) => Candidate {
                runtime,
                available: false,
                detail: reason,
            },
        })
        .collect();
    let recommended = recommend(&candidates);
    Report {
        platform,
        candidates,
        recommended,
    }
}

fn probe(runtime: Runtime) -> Result<String, String> {
    let (program, args) = runtime.probe_command();
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|error| format!("not found: {error}"))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if message.is_empty() {
            "present but not usable".to_owned()
        } else {
            message
        });
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(if text.is_empty() {
        "available".to_owned()
    } else {
        text.lines().next().unwrap_or("available").to_owned()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(entries: &[(Runtime, bool)]) -> Vec<Candidate> {
        entries
            .iter()
            .map(|(runtime, available)| Candidate {
                runtime: *runtime,
                available: *available,
                detail: String::new(),
            })
            .collect()
    }

    #[test]
    fn every_platform_orders_its_catalogue_strongest_first() {
        for platform in [Platform::MacOs, Platform::Linux, Platform::Windows] {
            let catalogue = catalog(platform);
            assert!(!catalogue.is_empty(), "{platform:?} has no candidates");
            let strengths: Vec<Isolation> = catalogue
                .iter()
                .map(|runtime| runtime.isolation())
                .collect();
            let mut sorted = strengths.clone();
            sorted.sort_by(|a, b| b.cmp(a));
            assert_eq!(strengths, sorted, "{platform:?} is not ordered by strength");
        }
    }

    #[test]
    fn an_unsupported_platform_offers_nothing_rather_than_guessing() {
        let report = detect_with(Platform::Other, &|_| Ok("x".to_owned()));
        assert!(report.candidates.is_empty());
        assert!(!report.can_contain());
    }

    #[test]
    fn the_strongest_available_runtime_wins() {
        let found = candidates(&[
            (Runtime::Firecracker, false),
            (Runtime::Podman, true),
            (Runtime::Docker, true),
        ]);
        assert_eq!(recommend(&found), Some(Runtime::Podman));
    }

    #[test]
    fn a_machine_with_nothing_installed_recommends_nothing() {
        let found = candidates(&[(Runtime::Podman, false), (Runtime::Docker, false)]);
        assert_eq!(recommend(&found), None);
        let report = detect_with(Platform::Linux, &|_| Err("not found".to_owned()));
        assert!(!report.can_contain());
        // And it still lists what it looked for, so the operator knows what
        // to install rather than being told "no".
        assert_eq!(report.candidates.len(), catalog(Platform::Linux).len());
    }

    #[test]
    fn a_probe_failure_is_recorded_as_a_reason_not_a_silence() {
        let report = detect_with(Platform::MacOs, &|runtime| {
            if runtime == Runtime::Podman {
                Ok("podman version 5.0.0".to_owned())
            } else {
                Err("not installed".to_owned())
            }
        });
        assert_eq!(report.recommended, Some(Runtime::Podman));
        let apple = report
            .candidates
            .iter()
            .find(|candidate| candidate.runtime == Runtime::AppleContainer)
            .unwrap();
        assert!(!apple.available);
        assert_eq!(apple.detail, "not installed");
    }

    /// Kernel-sharing and VM-backed runtimes must not be described as
    /// equivalent — this is the distinction the whole report exists to carry.
    #[test]
    fn isolation_strength_is_ordered_and_distinct() {
        assert!(Isolation::Machine > Isolation::Kernel);
        assert!(Isolation::Kernel > Isolation::None);
        assert_eq!(Runtime::Podman.isolation(), Isolation::Kernel);
        assert_eq!(Runtime::AppleContainer.isolation(), Isolation::Machine);
        assert_eq!(Runtime::Wsl2.isolation(), Isolation::Machine);
    }

    #[test]
    fn every_runtime_carries_a_caveat_and_a_probe() {
        for platform in [Platform::MacOs, Platform::Linux, Platform::Windows] {
            for runtime in catalog(platform) {
                assert!(!runtime.caveat().is_empty(), "{runtime:?}");
                assert!(!runtime.name().is_empty(), "{runtime:?}");
                let (program, _) = runtime.probe_command();
                assert!(!program.is_empty(), "{runtime:?}");
            }
        }
    }
}
