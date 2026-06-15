use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use bollard::Docker;
use bollard::container::InspectContainerOptions;
use bollard::container::RestartContainerOptions;
use bollard::container::StartContainerOptions;
use bollard::errors::Error as BollardError;
use bollard::models::ContainerStateStatusEnum;
use bollard::models::HealthStatusEnum;
use chrono::{DateTime, Utc};
use clap::Parser;
use serde::Deserialize;
use tokio::signal::unix::{SignalKind, signal};
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
    #[arg(
        long,
        env = "NERD_WATCH_POLL_INTERVAL",
        default_value = "5",
        value_name = "SECS"
    )]
    poll_interval: u64,

    /// Timeout in seconds given to a running container to shut down gracefully
    /// during a restart before Docker force-kills it.
    ///
    /// Can also be set via the NERD_WATCH_RESTART_TIMEOUT environment variable.
    #[arg(
        long,
        env = "NERD_WATCH_RESTART_TIMEOUT",
        default_value = "30",
        value_name = "SECS"
    )]
    restart_timeout: isize,

    /// Directory containing per-container maintenance marker files.
    ///
    /// When a file named <container>.json exists in this directory with a future
    /// expires_at timestamp, nerd-watch will continue polling but will not start
    /// or restart that container.
    #[arg(long, env = "NERD_WATCH_MAINTENANCE_DIR", value_name = "DIR")]
    maintenance_dir: Option<PathBuf>,
}

/// Runtime marker written by external maintenance jobs.
#[derive(Debug, Deserialize)]
struct MaintenanceMarker {
    /// RFC3339 timestamp after which the marker no longer suppresses restarts.
    expires_at: DateTime<Utc>,
    /// Optional human-readable reason included in logs.
    reason: Option<String>,
}

impl MaintenanceMarker {
    fn is_active(&self, now: DateTime<Utc>) -> bool {
        self.expires_at > now
    }
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

    /// Called when an external maintenance marker suppresses intervention.
    fn on_maintenance(&mut self) {
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
    let restart_timeout = args.restart_timeout;

    info!(
        containers = ?args.containers,
        poll_interval_secs = args.poll_interval,
        restart_timeout_secs = args.restart_timeout,
        maintenance_dir = ?args.maintenance_dir,
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

    let mut sigterm =
        signal(SignalKind::terminate()).context("Failed to register SIGTERM handler")?;

    loop {
        let docker = &docker;
        let maintenance_dir = args.maintenance_dir.as_deref();
        futures::future::join_all(states.iter_mut().map(|(name, state)| async move {
            if let Err(e) =
                poll_container(docker, name, state, restart_timeout, maintenance_dir).await
            {
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
async fn poll_container(
    docker: &Docker,
    name: &str,
    state: &mut ContainerState,
    restart_timeout: isize,
    maintenance_dir: Option<&Path>,
) -> Result<()> {
    match docker
        .inspect_container(name, None::<InspectContainerOptions>)
        .await
    {
        Ok(info) => {
            let container_state = info.state.as_ref();
            let status = container_state.and_then(|s| s.status.clone());
            let is_running = container_state.and_then(|s| s.running).unwrap_or(false);
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
                            if should_skip_for_maintenance(name, maintenance_dir)? {
                                state.on_maintenance();
                            } else {
                                attempt_restart_if_due(
                                    docker,
                                    name,
                                    state,
                                    RestartMode::Restart,
                                    restart_timeout,
                                )
                                .await?;
                            }
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
                    // UNHEALTHY and STARTING are handled above; everything else
                    // (HEALTHY, NONE, EMPTY, or no health check at all) is treated
                    // as healthy.
                    _ => {
                        state.on_healthy(name);
                    }
                },

                // Container exists but is stopped, exited, dead, or otherwise not running.
                Some(status_val) => {
                    state.on_stopped();
                    warn!(container = %name, status = ?status_val, "Container is not running");
                    if !should_skip_for_maintenance(name, maintenance_dir)? {
                        attempt_restart_if_due(
                            docker,
                            name,
                            state,
                            RestartMode::Start,
                            restart_timeout,
                        )
                        .await?;
                    }
                }

                // No status field — treat as stopped.
                None => {
                    state.on_stopped();
                    warn!(container = %name, "Container has no status — treating as stopped");
                    if !should_skip_for_maintenance(name, maintenance_dir)? {
                        attempt_restart_if_due(
                            docker,
                            name,
                            state,
                            RestartMode::Start,
                            restart_timeout,
                        )
                        .await?;
                    }
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

fn should_skip_for_maintenance(name: &str, maintenance_dir: Option<&Path>) -> Result<bool> {
    let Some(dir) = maintenance_dir else {
        return Ok(false);
    };

    let marker_path = dir.join(format!("{name}.json"));
    let marker_json = match fs::read_to_string(&marker_path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => {
            warn!(
                container = %name,
                marker = %marker_path.display(),
                error = %e,
                "Failed to read maintenance marker — ignoring marker"
            );
            return Ok(false);
        }
    };

    let marker: MaintenanceMarker = match serde_json::from_str(&marker_json) {
        Ok(marker) => marker,
        Err(e) => {
            warn!(
                container = %name,
                marker = %marker_path.display(),
                error = %e,
                "Failed to parse maintenance marker — ignoring marker"
            );
            return Ok(false);
        }
    };

    if marker.is_active(Utc::now()) {
        info!(
            container = %name,
            marker = %marker_path.display(),
            expires_at = %marker.expires_at.to_rfc3339(),
            reason = marker.reason.as_deref().unwrap_or("unspecified"),
            "Container is in maintenance mode — skipping restart"
        );
        return Ok(true);
    }

    info!(
        container = %name,
        marker = %marker_path.display(),
        expires_at = %marker.expires_at.to_rfc3339(),
        "Maintenance marker has expired — resuming normal restart behavior"
    );
    Ok(false)
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
    restart_timeout: isize,
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
                .restart_container(name, Some(RestartContainerOptions { t: restart_timeout }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as ChronoDuration;
    use std::time::{Duration, Instant};

    #[test]
    fn new_state_has_initial_defaults() {
        let state = ContainerState::new();
        assert_eq!(state.backoff, INITIAL_BACKOFF);
        assert_eq!(state.restart_count, 0);
        assert!(state.next_restart_at.is_none());
        assert!(state.stable_since.is_none());
        assert!(state.unhealthy_since.is_none());
    }

    #[test]
    fn should_restart_now_when_no_prior_attempt() {
        let state = ContainerState::new();
        assert!(state.should_restart_now());
    }

    #[test]
    fn should_not_restart_immediately_after_attempt() {
        let mut state = ContainerState::new();
        state.record_restart_attempt("test");
        // Immediately after recording, the backoff window is in the future.
        assert!(!state.should_restart_now());
    }

    #[test]
    fn backoff_doubles_and_caps_at_max() {
        let mut state = ContainerState::new();
        assert_eq!(state.backoff, Duration::from_secs(5));

        state.record_restart_attempt("test");
        assert_eq!(state.backoff, Duration::from_secs(10));

        state.record_restart_attempt("test");
        assert_eq!(state.backoff, Duration::from_secs(20));

        state.record_restart_attempt("test");
        assert_eq!(state.backoff, Duration::from_secs(40));

        // Drive it up to the cap.
        for _ in 0..10 {
            state.record_restart_attempt("test");
        }
        assert_eq!(state.backoff, MAX_BACKOFF);
    }

    #[test]
    fn restart_count_increments() {
        let mut state = ContainerState::new();
        assert_eq!(state.restart_count, 0);

        state.record_restart_attempt("test");
        assert_eq!(state.restart_count, 1);

        state.record_restart_attempt("test");
        assert_eq!(state.restart_count, 2);
    }

    #[test]
    fn on_stopped_clears_stable_and_unhealthy() {
        let mut state = ContainerState::new();
        state.stable_since = Some(Instant::now());
        state.unhealthy_since = Some(Instant::now());

        state.on_stopped();
        assert!(state.stable_since.is_none());
        assert!(state.unhealthy_since.is_none());
    }

    #[test]
    fn on_unhealthy_clears_stable_and_sets_unhealthy_since() {
        let mut state = ContainerState::new();
        state.stable_since = Some(Instant::now());
        assert!(state.unhealthy_since.is_none());

        state.on_unhealthy();
        assert!(state.stable_since.is_none());
        assert!(state.unhealthy_since.is_some());
    }

    #[test]
    fn on_unhealthy_does_not_reset_existing_timestamp() {
        let mut state = ContainerState::new();
        state.on_unhealthy();
        let first = state.unhealthy_since.unwrap();

        std::thread::sleep(Duration::from_millis(10));
        state.on_unhealthy();
        let second = state.unhealthy_since.unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn unhealthy_for_returns_none_when_healthy() {
        let state = ContainerState::new();
        assert!(state.unhealthy_for().is_none());
    }

    #[test]
    fn unhealthy_for_returns_duration_when_unhealthy() {
        let mut state = ContainerState::new();
        state.on_unhealthy();
        std::thread::sleep(Duration::from_millis(10));
        let dur = state.unhealthy_for().unwrap();
        assert!(dur >= Duration::from_millis(10));
    }

    #[test]
    fn on_health_starting_clears_both_timestamps() {
        let mut state = ContainerState::new();
        state.stable_since = Some(Instant::now());
        state.unhealthy_since = Some(Instant::now());

        state.on_health_starting();
        assert!(state.stable_since.is_none());
        assert!(state.unhealthy_since.is_none());
    }

    #[test]
    fn on_healthy_clears_unhealthy_since() {
        let mut state = ContainerState::new();
        state.on_unhealthy();
        assert!(state.unhealthy_since.is_some());

        state.on_healthy("test");
        assert!(state.unhealthy_since.is_none());
    }

    #[test]
    fn on_healthy_sets_stable_since() {
        let mut state = ContainerState::new();
        assert!(state.stable_since.is_none());

        state.on_healthy("test");
        assert!(state.stable_since.is_some());
    }

    #[test]
    fn on_healthy_does_not_reset_backoff_without_prior_restarts() {
        let mut state = ContainerState::new();
        // Simulate being stable for a long time without any restarts.
        state.stable_since = Some(Instant::now() - Duration::from_secs(120));
        state.on_healthy("test");

        // Backoff should stay at initial since restart_count is 0.
        assert_eq!(state.backoff, INITIAL_BACKOFF);
        assert_eq!(state.restart_count, 0);
    }

    #[test]
    fn on_healthy_resets_backoff_after_stability_threshold() {
        let mut state = ContainerState::new();
        // Simulate some restart attempts.
        state.record_restart_attempt("test");
        state.record_restart_attempt("test");
        assert_eq!(state.restart_count, 2);
        assert_eq!(state.backoff, Duration::from_secs(20));

        // Simulate being stable for longer than the threshold.
        state.stable_since = Some(Instant::now() - STABILITY_THRESHOLD - Duration::from_secs(1));
        state.on_healthy("test");

        assert_eq!(state.backoff, INITIAL_BACKOFF);
        assert_eq!(state.restart_count, 0);
        assert!(state.next_restart_at.is_none());
    }

    #[test]
    fn on_healthy_does_not_reset_backoff_before_stability_threshold() {
        let mut state = ContainerState::new();
        state.record_restart_attempt("test");
        state.record_restart_attempt("test");
        let backoff_before = state.backoff;
        let count_before = state.restart_count;

        // Stable for less than the threshold.
        state.stable_since = Some(Instant::now() - Duration::from_secs(10));
        state.on_healthy("test");

        assert_eq!(state.backoff, backoff_before);
        assert_eq!(state.restart_count, count_before);
    }

    #[test]
    fn should_restart_after_backoff_elapses() {
        let mut state = ContainerState::new();
        // Place the backoff window in the past.
        state.next_restart_at = Some(Instant::now() - Duration::from_secs(1));
        assert!(state.should_restart_now());
    }

    #[test]
    fn active_maintenance_marker_suppresses_restart() {
        let dir = unique_test_dir("active_maintenance_marker_suppresses_restart");
        fs::create_dir_all(&dir).unwrap();

        let expires_at = (Utc::now() + ChronoDuration::minutes(30)).to_rfc3339();
        fs::write(
            dir.join("test.json"),
            format!(r#"{{"expires_at":"{expires_at}","reason":"backup"}}"#),
        )
        .unwrap();

        assert!(should_skip_for_maintenance("test", Some(&dir)).unwrap());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn expired_maintenance_marker_does_not_suppress_restart() {
        let dir = unique_test_dir("expired_maintenance_marker_does_not_suppress_restart");
        fs::create_dir_all(&dir).unwrap();

        let expires_at = (Utc::now() - ChronoDuration::minutes(30)).to_rfc3339();
        fs::write(
            dir.join("test.json"),
            format!(r#"{{"expires_at":"{expires_at}","reason":"backup"}}"#),
        )
        .unwrap();

        assert!(!should_skip_for_maintenance("test", Some(&dir)).unwrap());

        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_maintenance_marker_does_not_suppress_restart() {
        let dir = unique_test_dir("missing_maintenance_marker_does_not_suppress_restart");
        fs::create_dir_all(&dir).unwrap();

        assert!(!should_skip_for_maintenance("test", Some(&dir)).unwrap());

        fs::remove_dir_all(dir).unwrap();
    }

    fn unique_test_dir(test_name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("nerd-watch-{test_name}-{}", std::process::id()))
    }
}
