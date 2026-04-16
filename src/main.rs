use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bollard::container::InspectContainerOptions;
use bollard::container::RestartContainerOptions;
use bollard::container::StartContainerOptions;
use bollard::errors::Error as BollardError;
use bollard::models::ContainerStateStatusEnum;
use bollard::models::HealthStatusEnum;
use bollard::Docker;
use clap::Parser;
use tokio::signal::unix::{signal, SignalKind};
use tracing::{error, info, warn};

const INITIAL_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(300); // 5 minutes
const STABILITY_THRESHOLD: Duration = Duration::from_secs(60);
const UNHEALTHY_THRESHOLD: Duration = Duration::from_secs(60);

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
    /// Current backoff duration — doubles after each restart attempt, capped at MAX_BACKOFF.
    backoff: Duration,
    /// Earliest time we are allowed to attempt the next restart.
    next_restart_at: Option<Instant>,
    /// Number of restart attempts since the last stability reset.
    restart_count: u32,
    /// When the container was first observed healthy enough to be considered stable.
    /// Used to detect that it has been stable long enough to reset the backoff.
    stable_since: Option<Instant>,
    /// When the container was first observed in the unhealthy state.
    unhealthy_since: Option<Instant>,
}

impl ContainerState {
    fn new() -> Self {
        Self {
            backoff: INITIAL_BACKOFF,
            next_restart_at: None,
            restart_count: 0,
            stable_since: None,
            unhealthy_since: None,
        }
    }

    /// Called when the container is observed as running and healthy enough to be considered stable.
    /// Resets the backoff once the container has been stable for STABILITY_THRESHOLD.
    fn on_healthy(&mut self, name: &str) {
        self.unhealthy_since = None;

        let now = Instant::now();
        let since = self.stable_since.get_or_insert(now);

        if self.restart_count > 0 && since.elapsed() >= STABILITY_THRESHOLD {
            info!(
                container = %name,
                uptime_secs = since.elapsed().as_secs(),
                "Container is stable — resetting restart backoff"
            );
            self.backoff = INITIAL_BACKOFF;
            self.restart_count = 0;
            self.next_restart_at = None;
        }
    }

    /// Called when the container is running but still bootstrapping its health checks.
    fn on_health_starting(&mut self) {
        self.stable_since = None;
        self.unhealthy_since = None;
    }

    /// Called when the container is observed as unhealthy.
    fn on_unhealthy(&mut self) {
        self.stable_since = None;
        self.unhealthy_since.get_or_insert_with(Instant::now);
    }

    /// Called when the container is observed as not running.
    fn on_stopped(&mut self) {
        self.stable_since = None;
        self.unhealthy_since = None;
    }

    /// Returns how long the container has been continuously unhealthy, if known.
    fn unhealthy_for(&self) -> Option<Duration> {
        self.unhealthy_since.map(|since| since.elapsed())
    }

    /// Returns true if the backoff window has elapsed and we can attempt a restart.
    fn should_restart_now(&self) -> bool {
        match self.next_restart_at {
            None => true,
            Some(at) => Instant::now() >= at,
        }
    }

    /// Records a restart attempt and arms the next backoff window.
    fn record_restart_attempt(&mut self, name: &str) {
        self.restart_count += 1;
        let current_backoff = self.backoff;
        self.next_restart_at = Some(Instant::now() + current_backoff);
        self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
        info!(
            container = %name,
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
        .map(|n| (n.clone(), ContainerState::new()))
        .collect();

    let mut sigterm = signal(SignalKind::terminate()).context("Failed to register SIGTERM handler")?;

    loop {
        let docker = &docker;
        futures::future::join_all(states.iter_mut().map(|(name, state)| async move {
            if let Err(e) = poll_container(docker, name, state).await {
                error!(
                    container = %name,
                    error = %e,
                    "Unexpected error while polling container"
                );
            }
        }))
        .await;

        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT — shutting down");
                break;
            }
            _ = sigterm.recv() => {
                info!("Received SIGTERM — shutting down");
                break;
            }
            _ = tokio::time::sleep(poll_interval) => {}
        }
    }

    info!("nerd-watch stopped");

    Ok(())
}

/// Inspects a container and intervenes when it is stopped or unhealthy.
async fn poll_container(docker: &Docker, name: &str, state: &mut ContainerState) -> Result<()> {
    match docker
        .inspect_container(name, None::<InspectContainerOptions>)
        .await
    {
        Ok(info) => {
            let container_state = info.state.as_ref();
            let status = container_state.and_then(|s| s.status.clone());
            let is_running = container_state
                .and_then(|s| s.running)
                .unwrap_or(false);
            let health_status = container_state
                .and_then(|s| s.health.as_ref())
                .and_then(|h| h.status);

            match status {
                // Docker is already handling the restart via its own restart policy.
                Some(ContainerStateStatusEnum::RESTARTING) => {
                    info!(container = %name, "Container is restarting (managed by Docker) — skipping");
                    state.on_stopped();
                }

                // Container is running; inspect health state when available.
                _ if is_running => match health_status {
                    Some(HealthStatusEnum::UNHEALTHY) => {
                        state.on_unhealthy();

                        let unhealthy_for = state.unhealthy_for().unwrap_or_default();
                        if unhealthy_for >= UNHEALTHY_THRESHOLD {
                            warn!(
                                container = %name,
                                unhealthy_secs = unhealthy_for.as_secs(),
                                threshold_secs = UNHEALTHY_THRESHOLD.as_secs(),
                                "Container is unhealthy long enough — attempting restart"
                            );
                            attempt_restart_if_due(docker, name, state, RestartMode::Restart).await?;
                        } else {
                            info!(
                                container = %name,
                                unhealthy_secs = unhealthy_for.as_secs(),
                                threshold_secs = UNHEALTHY_THRESHOLD.as_secs(),
                                "Container is unhealthy but still within grace period"
                            );
                        }
                    }
                    Some(HealthStatusEnum::STARTING) => {
                        state.on_health_starting();
                        info!(container = %name, "Container health is still starting — waiting");
                    }
                    Some(HealthStatusEnum::HEALTHY)
                    | Some(HealthStatusEnum::NONE)
                    | Some(HealthStatusEnum::EMPTY)
                    | None => {
                        state.on_healthy(name);
                    }
                },

                // Container exists but is stopped, exited, dead, or otherwise not running.
                Some(status_val) => {
                    state.on_stopped();
                    warn!(container = %name, status = ?status_val, "Container is not running");
                    attempt_restart_if_due(docker, name, state, RestartMode::Start).await?;
                }

                // No status field — treat as stopped.
                None => {
                    state.on_stopped();
                    warn!(container = %name, "Container has no status — treating as stopped");
                    attempt_restart_if_due(docker, name, state, RestartMode::Start).await?;
                }
            }
        }

        // Container does not exist; we cannot start what has never been created.
        Err(BollardError::DockerResponseServerError {
            status_code: 404, ..
        }) => {
            error!(
                container = %name,
                "Container not found — it must be created before nerd-watch can restart it"
            );
        }

        Err(e) => {
            return Err(e).context(format!("Failed to inspect container '{name}'"));
        }
    }

    Ok(())
}

enum RestartMode {
    /// Container is stopped — use `docker start`.
    Start,
    /// Container is running but unhealthy — use `docker restart`.
    Restart,
}

async fn attempt_restart_if_due(
    docker: &Docker,
    name: &str,
    state: &mut ContainerState,
    mode: RestartMode,
) -> Result<()> {
    if !state.should_restart_now() {
        let wait = state
            .next_restart_at
            .map(|t| t.saturating_duration_since(Instant::now()))
            .unwrap_or_default();
        info!(
            container = %name,
            wait_secs = wait.as_secs(),
            "Backoff in effect — waiting before next restart attempt"
        );
        return Ok(());
    }

    let action = match mode {
        RestartMode::Start => "start stopped container",
        RestartMode::Restart => "restart unhealthy container",
    };
    info!(
        container = %name,
        attempt = state.restart_count + 1,
        "Attempting to {action}"
    );

    match mode {
        RestartMode::Start => {
            match docker
                .start_container(name, None::<StartContainerOptions<String>>)
                .await
            {
                Ok(()) => info!(container = %name, "Container started successfully"),
                Err(BollardError::DockerResponseServerError {
                    status_code: 304, ..
                }) => {
                    info!(container = %name, "Container was already running");
                }
                Err(e) => {
                    error!(container = %name, error = %e, "Failed to start container");
                }
            }
        }
        RestartMode::Restart => {
            match docker
                .restart_container(name, Some(RestartContainerOptions { t: 30 }))
                .await
            {
                Ok(()) => info!(container = %name, "Container restarted successfully"),
                Err(e) => {
                    error!(
                        container = %name, error = %e,
                        "Failed to restart unhealthy container"
                    );
                }
            }
        }
    }

    state.record_restart_attempt(name);
    Ok(())
}
