# nerd-watch

`nerd-watch` is a small Docker container watchdog written in Rust. It monitors one or more existing Docker containers and attempts to start them again when they stop running.

It is designed for cases where you want an external watcher process that:

- polls container state through the Docker socket
- restarts stopped containers
- restarts running containers that remain unhealthy for too long
- applies exponential backoff between restart attempts
- resets the backoff after a container has stayed healthy for a while
- can run as its own Docker service

## How it works

For each watched container, `nerd-watch`:

- checks the container status on a configurable poll interval
- ignores containers already in Docker's `restarting` state
- attempts to start containers that are stopped, exited, dead, or otherwise not running
- restarts running containers whose Docker health status remains `unhealthy` for 60 seconds
- does not restart containers whose Docker health status is still `starting`
- uses exponential backoff starting at 5 seconds and capping at 5 minutes
- resets the backoff after the container has remained stable for 60 seconds

If a container does not exist, `nerd-watch` logs an error and keeps running.

For health-based restarts to work, the watched container must define a Docker health check. Containers without a health check are treated as healthy as long as they are running.

## Configuration

`nerd-watch` supports both CLI arguments and environment variables.

### Environment variables

- `NERD_WATCH_CONTAINERS`: comma-separated list of container names to watch
- `NERD_WATCH_POLL_INTERVAL`: poll interval in seconds
- `NERD_WATCH_MAINTENANCE_DIR`: optional directory containing per-container maintenance markers
- `RUST_LOG`: optional Rust log filter, for example `info` or `debug`

Health-based restart behavior is always enabled. A running container is restarted only after it has remained Docker-`unhealthy` for 60 seconds. Containers in Docker health state `starting` are left alone.

Example:

```dotenv
NERD_WATCH_CONTAINERS=my-app,my-worker
NERD_WATCH_POLL_INTERVAL=5
NERD_WATCH_MAINTENANCE_DIR=/run/nerd-watch/maintenance
RUST_LOG=info
```

### CLI usage

```bash
nerd-watch --poll-interval 5 my-app my-worker
```

Or with environment variables:

```bash
NERD_WATCH_CONTAINERS=my-app,my-worker \
NERD_WATCH_POLL_INTERVAL=5 \
NERD_WATCH_MAINTENANCE_DIR=/run/nerd-watch/maintenance \
nerd-watch
```

## Maintenance markers

External maintenance jobs can temporarily suppress restarts for watched
containers by writing marker files into `NERD_WATCH_MAINTENANCE_DIR`.

For a watched container named `my-app`, create:

```text
/run/nerd-watch/maintenance/my-app.json
```

With content:

```json
{
  "expires_at": "2026-06-15T12:30:00Z",
  "reason": "volume backup"
}
```

While `expires_at` is in the future, `nerd-watch` continues polling the
container but does not start it when stopped and does not restart it when
unhealthy. Once the marker expires, normal watchdog behavior resumes. Delete
the marker when maintenance finishes.

Backup jobs should write markers atomically, for example by writing
`my-app.json.tmp` first and then renaming it to `my-app.json`, so
`nerd-watch` never sees a partially written file.

Example backup flow:

```bash
printf '%s\n' '{"expires_at":"2026-06-15T12:30:00Z","reason":"volume backup"}' \
  > /run/nerd-watch/maintenance/my-app.json.tmp
mv /run/nerd-watch/maintenance/my-app.json.tmp \
  /run/nerd-watch/maintenance/my-app.json

docker stop my-app
# Back up volumes here.
docker start my-app

rm -f /run/nerd-watch/maintenance/my-app.json
```

## Local development

### Build

```bash
cargo build
```

### Run locally

`nerd-watch` needs access to the Docker socket:

```bash
NERD_WATCH_CONTAINERS=my-app,my-worker \
NERD_WATCH_POLL_INTERVAL=5 \
cargo run --release
```

On a typical Linux host, this assumes the local Docker socket is available at `/var/run/docker.sock`.

### Verify

```bash
cargo check
```

## Docker image build

A multi-stage `Dockerfile` is included.

### Build the image locally

```bash
docker build -t nerd-watch:latest .
```

### Run the image directly

```bash
docker run -d \
  --name nerd-watch \
  --restart unless-stopped \
  --env NERD_WATCH_CONTAINERS=my-app,my-worker \
  --env NERD_WATCH_POLL_INTERVAL=5 \
  --volume /var/run/docker.sock:/var/run/docker.sock \
  nerd-watch:latest
```

## Deploy with Docker Compose

The repository includes a `docker-compose.yml` that uses the published image:

```yaml
services:
  nerd-watch:
    image: ghcr.io/avranju/nerd-watch:latest
    container_name: nerd-watch
    restart: unless-stopped
    env_file:
      - .env
    volumes:
      - /var/run/docker.sock:/var/run/docker.sock
```

### 1. Create a `.env` file

```bash
cp .env.example .env
```

Then edit `.env`:

```dotenv
NERD_WATCH_CONTAINERS=my-app,my-worker
NERD_WATCH_POLL_INTERVAL=5
RUST_LOG=info
```

### 2. Start the service

```bash
docker compose up -d
```

### 3. View logs

```bash
docker compose logs -f nerd-watch
```

### 4. Stop the service

```bash
docker compose down
```

## Publishing the image

To publish the image to the configured registry:

```bash
docker build -t ghcr.io/avranju/nerd-watch:latest .
docker push ghcr.io/avranju/nerd-watch:latest
```

If you want versioned releases, tag and push an additional version tag:

```bash
docker build \
  -t ghcr.io/avranju/nerd-watch:latest \
  -t ghcr.io/avranju/nerd-watch:v0.1.0 \
  .

docker push ghcr.io/avranju/nerd-watch:latest
docker push ghcr.io/avranju/nerd-watch:v0.1.0
```

## Notes

- `nerd-watch` watches existing containers by name; it does not create them.
- The Docker socket mount gives the watcher control over the local Docker daemon. Only deploy it in environments where that is acceptable.
- Docker's own restart policies may already solve some use cases. `nerd-watch` is most useful when you want an external watchdog with explicit polling and backoff behavior.
