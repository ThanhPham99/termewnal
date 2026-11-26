//! Docker and Podman container discovery and interactive `exec -it` [`ShellSpec`] generation.

use std::process::Command;

use termewnal_core::{DockerRuntime, ProfileTarget};
use termewnal_pty::ShellSpec;

use crate::ProtocolError;

/// Discovered local container metadata returned by [`DockerDiscovery`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    pub runtime: DockerRuntime,
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
}

/// Launch parameters for attaching a local PTY session to a running container via `exec -it`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerExecConfig {
    pub runtime: DockerRuntime,
    pub container: String,
    pub shell: Option<String>,
    pub user: Option<String>,
    pub working_dir: Option<String>,
}

impl ContainerExecConfig {
    /// Extracts container attach settings from [`ProfileTarget::Docker`].
    pub fn from_profile_target(target: &ProfileTarget) -> Option<Self> {
        match target {
            ProfileTarget::Docker {
                runtime,
                container,
                shell,
                user,
                working_dir,
            } => Some(Self {
                runtime: *runtime,
                container: container.clone(),
                shell: shell.clone(),
                user: user.clone(),
                working_dir: working_dir.clone(),
            }),
            _ => None,
        }
    }
}

/// Injectable CLI runner for `docker ps` / `podman ps`.
pub trait CommandOutputRunner: Send + Sync {
    fn run_stdout(&self, program: &str, args: &[&str]) -> Result<String, ProtocolError>;
}

/// OS subprocess implementation of [`CommandOutputRunner`].
#[derive(Debug, Default)]
pub struct OsCommandRunner;

impl CommandOutputRunner for OsCommandRunner {
    fn run_stdout(&self, program: &str, args: &[&str]) -> Result<String, ProtocolError> {
        let output = Command::new(program).args(args).output().map_err(|err| {
            ProtocolError::Docker(format!("failed running `{program}`: {err}"))
        })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(ProtocolError::Docker(format!(
                "`{program}` exited with {}: {}",
                output.status,
                stderr.trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

/// Discovers running containers across Docker and Podman and constructs interactive `ShellSpec`s.
pub struct DockerDiscovery {
    runner: Box<dyn CommandOutputRunner>,
}

impl Default for DockerDiscovery {
    fn default() -> Self {
        Self::new(Box::new(OsCommandRunner))
    }
}

impl DockerDiscovery {
    pub fn new(runner: Box<dyn CommandOutputRunner>) -> Self {
        Self { runner }
    }

    /// Queries `docker ps` or `podman ps` and parses container rows.
    pub fn list_containers(
        &self,
        runtime: DockerRuntime,
        include_stopped: bool,
    ) -> Result<Vec<ContainerInfo>, ProtocolError> {
        let bin = runtime_binary(runtime);
        let mut args = vec!["ps"];
        if include_stopped {
            args.push("-a");
        }
        args.extend(["--format", "{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.Status}}"]);
        let stdout = self.runner.run_stdout(bin, &args)?;
        Ok(parse_ps_output(runtime, &stdout))
    }

    /// Queries both `docker` and `podman`, ignoring unavailable runtimes cleanly.
    pub fn list_all_containers(&self, include_stopped: bool) -> Vec<ContainerInfo> {
        let mut all = Vec::new();
        for runtime in [DockerRuntime::Docker, DockerRuntime::Podman] {
            if let Ok(mut items) = self.list_containers(runtime, include_stopped) {
                all.append(&mut items);
            }
        }
        all
    }
}

/// Parses tab-delimited `docker ps` / `podman ps` formatted lines into [`ContainerInfo`].
pub fn parse_ps_output(runtime: DockerRuntime, stdout: &str) -> Vec<ContainerInfo> {
    let mut out = Vec::new();
    for raw_line in stdout.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(4, '\t');
        let id = parts.next().unwrap_or_default().trim().to_string();
        let name = parts.next().unwrap_or_default().trim().to_string();
        let image = parts.next().unwrap_or_default().trim().to_string();
        let status = parts.next().unwrap_or_default().trim().to_string();
        if id.is_empty() && name.is_empty() {
            continue;
        }
        out.push(ContainerInfo {
            runtime,
            id,
            name,
            image,
            status,
        });
    }
    out
}

/// Builds a [`ShellSpec`] invoking `<docker|podman> exec -it ...` for local PTY execution.
pub fn build_exec_shell_spec(config: &ContainerExecConfig) -> Result<ShellSpec, ProtocolError> {
    let container = config.container.trim();
    if container.is_empty() {
        return Err(ProtocolError::InvalidInput(
            "container name or ID must not be empty".to_string(),
        ));
    }

    let bin = runtime_binary(config.runtime);
    let mut args = vec!["exec".to_string(), "-it".to_string()];

    if let Some(user) = config
        .user
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.push("-u".to_string());
        args.push(user.to_string());
    }
    if let Some(workdir) = config
        .working_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.push("-w".to_string());
        args.push(workdir.to_string());
    }

    args.push(container.to_string());

    if let Some(shell) = config
        .shell
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        args.push(shell.to_string());
    } else {
        args.push("/bin/sh".to_string());
        args.push("-c".to_string());
        args.push("if command -v bash >/dev/null 2>&1; then exec bash; else exec sh; fi".to_string());
    }

    let mut spec = ShellSpec::custom(bin, args);
    spec.display_name = format!("{bin}: {container}");
    Ok(spec)
}

fn runtime_binary(runtime: DockerRuntime) -> &'static str {
    match runtime {
        DockerRuntime::Docker => "docker",
        DockerRuntime::Podman => "podman",
    }
}
