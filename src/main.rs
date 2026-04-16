use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bollard::container::InspectContainerOptions;
use bollard::container::StartContainerOptions;
use bollard::errors::Error as BollardError;
use bollard::models::ContainerStateStatusEnum;
use bollard::Docker;
use clap::Parser;
use tracing::{error, info, warn};

const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300); // 5 minutes
const STABILITY_THRESHOLD: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(
    name = "nerd-watch",
    about = "Docker container watchdog — monitors containers and restarts them when they die"
)]
struct Args {
    /// Names of containers to watch.
    ///
    /// Can be provided either as positional CLI arguments or via the
    /// NERD_WATCH_CONTAINERS environment variable as a comma-separated list.
    #[arg(
        value_name = "CONTAINER",
        env = "NERD_WATCH_CONTAINERS",
        value_delimiter = ',',
        required = true
    )]
    containers: Vec<String>,

    /// How often to poll container status, in seconds.
    ///
    /// Can also be set via the NERD_WATCH_POLL_INTERVAL environment variable.
    #[arg(long, env = "NERD_WATCH_POLL_INTERVAL", default_value = "5", value_name = "SECS")]
    poll_interval: u64,
}

/// Per-container tracking state.
struct ContainerState {
    name: String,
    /// Current backoff duration — doubles after each restart attempt, capped at MAX_BACKOFF.
    backoff: Duration,
    /// Earliest time we are allowed to attempt the next restart.
    next_restart_at: Option<Instant>,
    /// Number of restart attempts since the last stability reset.
    restart_count: u32,
    /// When the container was first observed running in the current uptime period.
    /// Used to detect that it has been stable long enough to reset the backoff.
    stable_since: Option<Instant>,
}

impl ContainerState {
    fn new(name: String) -> Self {
        Self {
            name,
            backoff: INITIAL_BACKOFF,
            next_restart_at: None,
            restart_count: 0,
            stable_since: None,
        }
    }

    /// Called when the container is observed as running.
    /// Resets the backoff once the container has been stable for STABILITY_THRESHOLD.
    fn on_running(&mut self) {
        let now = Instant::now();
        let since = self.stable_since.get_or_insert(now);

        if self.restart_count > 0 && since.elapsed() >= STABILITY_THRESHOLD {
            info!(
                container = %self.name,
                uptime_secs = since.elapsed().as_secs(),
                "Container is stable — resetting restart backoff"
            );
            self.backoff = INITIAL_BACKOFF;
            self.restart_count = 0;
            self.next_restart_at = None;
        }
    }

    /// Called when the container is observed as not running.
    fn on_stopped(&mut self) {
        self.stable_since = None;
    }

    /// Returns true if the backoff window has elapsed and we can attempt a restart.
    fn should_restart_now(&self) -> bool {
        match self.next_restart_at {
            None => true,
            Some(at) => Instant::now() >= at,
        }
    }

    /// Records a restart attempt and arms the next backoff window.
    fn record_restart_attempt(&mut self) {
        self.restart_count += 1;
        let current_backoff = self.backoff;
        self.next_restart_at = Some(Instant::now() + current_backoff);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        info!(
            container = %self.name,
            attempt = self.restart_count,
            backoff_secs = current_backoff.as_secs(),
            next_backoff_secs = self.backoff.as_secs(),
            "Restart attempt recorded; next window opens in {} s",
            current_backoff.as_secs()
        );
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let poll_interval = Duration::from_secs(args.poll_interval);

    info!(
        containers = ?args.containers,
        poll_interval_secs = args.poll_interval,
        "nerd-watch starting"
    );

    let docker = Docker::connect_with_socket_defaults()
        .context("Failed to connect to Docker socket — is /var/run/docker.sock mounted?")?;

    let version_info = docker
        .version()
        .await
        .context("Failed to query Docker version — is the daemon running?")?;
    info!(
        docker_version = %version_info.version.as_deref().unwrap_or("unknown"),
        "Connected to Docker"
    );

    let mut states: HashMap<String, ContainerState> = args
        .containers
        .into_iter()
        .map(|n| (n.clone(), ContainerState::new(n)))
        .collect();

    loop {
        for state in states.values_mut() {
            if let Err(e) = poll_container(&docker, state).await {
                error!(
                    container = %state.name,
                    error = %e,
                    "Unexpected error while polling container"
                );
            }
        }
        tokio::time::sleep(poll_interval).await;
    }
}

/// Inspects a container and restarts it if it is not running and the backoff window has elapsed.
async fn poll_container(docker: &Docker, state: &mut ContainerState) -> Result<()> {
    match docker
        .inspect_container(&state.name, None::<InspectContainerOptions>)
        .await
    {
        Ok(info) => {
            let container_state = info.state.as_ref();
            let status = container_state.and_then(|s| s.status.clone());
            let is_running = container_state
                .and_then(|s| s.running)
                .unwrap_or(false);

            match status {
                // Docker is already handling the restart via its own restart policy.
                Some(ContainerStateStatusEnum::RESTARTING) => {
                    info!(container = %state.name, "Container is restarting (managed by Docker) — skipping");
                    state.on_stopped();
                }

                // Container is alive and well.
                _ if is_running => {
                    state.on_running();
                }

                // Container exists but is stopped, exited, dead, or otherwise not running.
                Some(status_val) => {
                    state.on_stopped();
                    warn!(container = %state.name, status = ?status_val, "Container is not running");
                    attempt_restart_if_due(docker, state).await?;
                }

                // No status field — treat as stopped.
                None => {
                    state.on_stopped();
                    warn!(container = %state.name, "Container has no status — treating as stopped");
                    attempt_restart_if_due(docker, state).await?;
                }
            }
        }

        // Container does not exist; we cannot start what has never been created.
        Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            error!(
                container = %state.name,
                "Container not found — it must be created before nerd-watch can restart it"
            );
        }

        Err(e) => {
            return Err(e).context(format!("Failed to inspect container '{}'", state.name));
        }
    }

    Ok(())
}

/// Calls `docker start` if the backoff window has elapsed; otherwise logs the remaining wait.
async fn attempt_restart_if_due(docker: &Docker, state: &mut ContainerState) -> Result<()> {
    if !state.should_restart_now() {
        let wait = state
            .next_restart_at
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        info!(
            container = %state.name,
            wait_secs = wait.as_secs(),
            "Backoff in effect — waiting before next restart attempt"
        );
        return Ok(());
    }

    info!(
        container = %state.name,
        attempt = state.restart_count + 1,
        "Attempting to start container"
    );

    match docker
        .start_container(&state.name, None::<StartContainerOptions<String>>)
        .await
    {
        Ok(()) => {
            info!(container = %state.name, "Container started successfully");
        }
        // 304 Not Modified — container was already running (race with another process).
        Err(BollardError::DockerResponseServerError {
            status_code: 304, ..
        }) => {
            info!(container = %state.name, "Container was already running");
        }
        Err(e) => {
            error!(container = %state.name, error = %e, "Failed to start container");
        }
    }

    // Record the attempt regardless of outcome so the backoff applies to the next cycle.
    state.record_restart_attempt();

    Ok(())
}
