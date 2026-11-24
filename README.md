# Potluck

A self-hosted team server for sharing workspaces of local-first desktop apps, git-style. Its first clients are [Apinata](https://github.com/PhongGuy/apinata), a native API client, and [Dbento](https://github.com/PhongGuy/dbento), a native database client. It runs as one container under podman or docker.

This page is for people who run Potluck. To build it from source or work on it, see [CONTRIBUTING.md](CONTRIBUTING.md).

## How it works

A team shares its work in workspaces. Each workspace is a bare git repository on the server, served over git's smart HTTP protocol by the `git` binary in the image. The repositories are plain git, so if you stop running Potluck you keep every workspace with its full history.

- **Kinds.** Each workspace has a kind that names the app whose files it holds: `apinata` or `dbento`. Each app owns the push rules and the starting files for its kind, in a small rules crate in the app's own repository. Potluck builds against each rules crate at a pinned git tag and knows no app file format itself.
- **Formats.** `GET /api/v1/server` lists each kind with the workspace formats this server accepts. A push in a format the server does not list is refused.
- **Push check.** Secrets stay on each person's machine. Before git accepts a push, a pre-receive check scans the objects the push adds. It refuses files named exactly `.env` (`.env.example` and other variants pass), private keys, and AWS, GitHub, Slack and Potluck tokens, plus whatever the workspace's kind rules refuse. Only a workspace admin, or a push with an armed override, can force-push or delete the workspace's default branch.
- **Overrides.** A workspace admin can arm an override that lets one person's next push through the check. Arming, disarming and using an override are logged, and a used override stays in the workspace's history.
- **Accounts and roles.** People sign in with a local password account and use personal access tokens for git, the JSON API and automation. Each workspace grants a user or a team one of three roles: read (clone and fetch), write (also push) or admin (also manage grants and overrides).
- **Change notices.** `GET /api/v1/workspaces/{team}/{ws}/events` is a server-sent events stream that carries only which ref now points at which commit; apps then fetch the commits over git. An app that cannot keep the stream open can poll instead.
- **Git access.** Apps clone, fetch and push a workspace at `<public URL>/git/{team}/{ws}`, signing in with a personal access token. Only a workspace admin, or a push with an armed override, can write refs outside branches and tags (`refs/heads/*` and `refs/tags/*`). To import an existing repository without the admin role, push `'refs/heads/*:refs/heads/*' 'refs/tags/*:refs/tags/*'`; a `--mirror` push of a clone carries `refs/remotes/*` and is refused.
- **Administration.** `potluck admin` on the host manages users, tokens, teams, workspaces, grants, overrides and update checks, and reads the audit log (see [First admin](#first-admin)). The same operations are in the JSON API under `/api/v1`. There is no web UI.

## Status

Potluck is in its 0.x releases and may still change in ways that break existing setups. Read each release's notes before you update.

## Run it

The image is `ghcr.io/phongguy/potluck:latest`, built for amd64 and arm64, with each release also tagged by its version, such as `v0.1.0`. It runs as a non-root user (uid 65532) and keeps everything it stores in one volume mounted at `/data`.

The image starts potluck under tini, a minimal init that reaps stray processes and passes stop signals on, so it needs no `--init` flag or `init: true`.

To build the image yourself instead, from a checkout of this repository, use the same name so the examples below find it locally:

```sh
podman build -t ghcr.io/phongguy/potluck:latest .
```

### One command

With podman:

```sh
podman run -d --name potluck -p 127.0.0.1:8080:8080 -v potluck-data:/data -e POTLUCK_PUBLIC_URL=http://127.0.0.1:8080 ghcr.io/phongguy/potluck:latest
```

With docker:

```sh
docker run -d --name potluck -p 127.0.0.1:8080:8080 -v potluck-data:/data -e POTLUCK_PUBLIC_URL=http://127.0.0.1:8080 ghcr.io/phongguy/potluck:latest
```

The server now answers on `http://127.0.0.1:8080`, on this machine only. `POTLUCK_PUBLIC_URL` is the address your team's apps use to reach the server. Once other people connect, set it to your reverse proxy's `https://` address (see [Reverse proxy and TLS](#reverse-proxy-and-tls)). The server refuses a plain `http://` address on anything but this machine, because access tokens would then cross the network in clear; `POTLUCK_INSECURE_HTTP=1` overrides that, for a trusted network only.

Every setting is a `potluck serve` flag with a `POTLUCK_*` environment variable, among them the push size limit (`POTLUCK_MAX_PUSH_BYTES`, 100 MiB by default) and the log filter (`POTLUCK_LOG`). `podman exec potluck potluck serve --help` lists them all.

### Compose

[`compose.yaml`](compose.yaml) runs the same container with a hardened setup (read-only root, no capabilities) and restarts it after a reboot. Put a file named `.env` next to it with one line, your public address:

```sh
POTLUCK_PUBLIC_URL=https://potluck.example.com
```

With docker:

```sh
docker compose up -d
```

With podman, `podman compose` hands the file to a compose provider that talks to podman's user socket, so enable that socket once first:

```sh
systemctl --user enable --now podman.socket
podman compose up -d
```

If the `potluck-data` volume already exists, from the one-command run or a restore, `up` warns `volume "potluck-data" already exists but was not created by Docker Compose`. That is expected: compose uses the existing volume as it is.

### Quadlet (podman under systemd)

[`deploy/potluck.container`](deploy/potluck.container) runs Potluck as a systemd user service under rootless podman. Open it, change the line `Environment=POTLUCK_PUBLIC_URL=https://potluck.example.com` to your public address, then copy it into place and start it.

With podman:

```sh
mkdir -p ~/.config/containers/systemd
cp deploy/potluck.container ~/.config/containers/systemd/
systemctl --user daemon-reload
systemctl --user start potluck
```

To keep it running after you log out, let your user's services run without a session:

```sh
loginctl enable-linger "$USER"
```

## First admin

Accounts are created from the host's shell, never over the network: `potluck admin` runs inside the container and talks to the server over the socket in `/data`. Create the first server admin and let Potluck choose the password:

```sh
podman exec potluck potluck admin user create alice --server-admin
```

With docker: `docker exec potluck potluck admin user create alice --server-admin`. The command prints the account and, once, its password; nothing shows it again. To pick the password yourself, pipe it in as one line:

```sh
podman exec -i potluck potluck admin user create alice --server-admin --password-stdin < alice-password.txt
```

The admin then signs in from the app, or with any HTTP client: `POST /api/v1/session` with `{"username", "password", "token_name"}` answers a token. `potluck admin --help` lists the rest: users, tokens, teams, workspaces, grants, overrides, update checks and audit; `--json` prints the answer as JSON instead of a table. Audit rows of changes are kept 365 days and rows of failed sign-ins 30 days; `--audit-retention-days` and `--audit-failed-retention-days` (or `POTLUCK_AUDIT_RETENTION_DAYS` and `POTLUCK_AUDIT_FAILED_RETENTION_DAYS`) change that, and 0 keeps rows forever.

## Reverse proxy and TLS

Potluck speaks plain HTTP and expects a reverse proxy in front of it to provide HTTPS; built-in TLS comes later. Set `POTLUCK_PUBLIC_URL` to the proxy's `https://` address, for example `https://potluck.example.com`.

Caddy gets a certificate on its own and streams the server's live change notices without extra settings. In the Caddyfile:

```caddyfile
potluck.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

With nginx, inside the `server` block that already holds your certificate:

```nginx
location / {
    proxy_pass http://127.0.0.1:8080;
    proxy_http_version 1.1;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto $scheme;
    # With HTTP/1.1 to the server, live change notices and large pushes stream through unbuffered.
    proxy_buffering off;
    proxy_request_buffering off;
    proxy_read_timeout 1h;
    # Pushes may be up to 100 MiB.
    client_max_body_size 110m;
}
```

The server trusts `X-Forwarded-For` only from the addresses listed in `POTLUCK_TRUSTED_PROXIES` (comma-separated addresses or CIDRs; default: none), so put your proxy's address there. Sign-in attempts are rate limited by client address, and behind a proxy that is not listed every attempt looks like it comes from the proxy.

## Updating

The server checks GitHub once a day for a new signed release. When one exists it logs a line and shows it to server admins; it never updates itself, and the check is a plain HTTPS request carrying only the server's version. `potluck admin update check` checks now and prints the update steps pinned to the release's image digest. To turn the daily check off, pass `--no-update-check` or set `POTLUCK_UPDATE_CHECK=false`.

With podman, pull the new image and recreate the container:

```sh
podman pull ghcr.io/phongguy/potluck:latest
podman rm -f potluck
podman run -d --name potluck -p 127.0.0.1:8080:8080 -v potluck-data:/data -e POTLUCK_PUBLIC_URL=http://127.0.0.1:8080 ghcr.io/phongguy/potluck:latest
```

With docker, the same three commands with `docker` in place of `podman`. Your data stays in the `potluck-data` volume.

With the Quadlet unit, podman can check for a new image and restart the service for you:

```sh
podman auto-update
```

To do that daily, enable podman's timer once:

```sh
systemctl --user enable --now podman-auto-update.timer
```

With compose (docker shown; `podman compose` takes the same words):

```sh
docker compose pull
docker compose up -d
```

Outside a container, restart the server after replacing its binary. The push check runs the binary at the path the server started from, so a binary replaced while the server runs checks pushes for an older server until the restart. The container commands above restart it for you.

## Metrics

The server can serve Prometheus metrics on a second port. It is off unless you set `--metrics-listen` or `POTLUCK_METRICS_LISTEN`. Inside the container, listen on all interfaces and publish the port on the host's loopback only:

```sh
podman run -d --name potluck -p 127.0.0.1:8080:8080 -e POTLUCK_METRICS_LISTEN=0.0.0.0:9464 -p 127.0.0.1:9464:9464 -v potluck-data:/data -e POTLUCK_PUBLIC_URL=http://127.0.0.1:8080 ghcr.io/phongguy/potluck:latest
```

`GET /metrics` on that port answers in the OpenMetrics text format; every other path answers 404. It has no authentication, so keep the port off the public network and do not route it through your reverse proxy. The metrics carry no user names, tokens, team or workspace names.

These families record today:

- `potluck_build_info`: the running version.
- `http_server_request_duration_seconds`: a histogram of API and git requests by method, route template and status code. The change-notice stream is left out.
- `potluck_auth_failures_total`: requests answered 401 although they carried credentials, by scheme.
- `potluck_sign_ins_total`: password sign-ins by outcome (`success`, `failure`, `rate_limited_ip`, `rate_limited_username`).
- `potluck_sse_subscribers`: open change-notice streams.
- `potluck_git_slots_in_use` and `potluck_git_slots_limit`: git slots held now and configured.
- `potluck_db_queue_depth`: database jobs waiting to run.
- `potluck_update_available` and `potluck_update_checks_total`: whether a newer release was found, and update checks by outcome.
- `potluck_git_requests_total`: git requests that reached the git engine, by service (`advertise`, `upload_pack`, `receive_pack`) and outcome (`ok`, `busy`, `overran`, `failed`).
- `potluck_git_slot_wait_seconds`: a histogram of how long each git request waited for a slot, whether or not it got one.
- `potluck_pushes_total`: pushes by outcome (`accepted`, `refused`).
- `potluck_push_refusals_total`: refusal findings from the push check, by code, rule and workspace kind. Past 128 distinct rules, new ones record as `other`.

Any Prometheus-compatible scraper reads it. With Grafana Alloy, the default `prometheus.scrape` settings accept OpenMetrics:

```alloy
prometheus.scrape "potluck" {
  targets    = [{"__address__" = "127.0.0.1:9464"}]
  forward_to = [prometheus.remote_write.default.receiver]
}
```

The example assumes a `prometheus.remote_write "default"` component is defined elsewhere in the Alloy config.

## Backups

Everything lives in the `potluck-data` volume: the database `/data/server.db` (with its `server.db-wal` and `server.db-shm` files) and the workspaces' git repositories under `/data/repos`. The one-liners, `compose.yaml` and the Quadlet unit all use that same volume name.

`potluck admin backup` copies a running server without stopping it. It writes a consistent tar of the database and the repositories to stdout, and prints a summary line on stderr, plus a `left out:` line for each workspace created or deleted while it ran. Run it without `-t`, because a terminal would corrupt the tar; the command refuses to write to one. Writing to a `.part` file and renaming it only on success keeps a failed run from replacing a good backup:

```sh
podman exec potluck potluck admin backup > potluck-data.tar.part && mv potluck-data.tar.part potluck-data.tar
```

With docker:

```sh
docker exec potluck potluck admin backup > potluck-data.tar.part && mv potluck-data.tar.part potluck-data.tar
```

The copy is staged inside the volume first, so the volume needs room for a second copy of the data plus a reserve; `--reserve-bytes` (or `POTLUCK_BACKUP_RESERVE_BYTES`) sets that reserve. The tar holds the password and token hashes, so store it as carefully as the volume itself. Armed overrides are not kept: the backup disarms every armed override that has not been used yet, so after a restore an admin re-arms any override still needed. The tar restores with the same steps as an offline backup, below.

To back up offline instead, stop the server so the copy is consistent, export the volume, then start it again.

With podman, for a container started with `podman run` or `podman compose`:

```sh
podman stop potluck
podman volume export potluck-data > potluck-data.tar
podman start potluck
```

With docker, which has no volume export, copy it out through a throwaway container:

```sh
docker stop potluck
docker run --rm -v potluck-data:/data -v "$PWD":/backup debian:trixie-slim tar -C /data -cf /backup/potluck-data.tar .
docker start potluck
```

With the Quadlet unit, stop and start the service through systemd instead, because a plain `podman stop` lets the unit restart the container in the middle of the copy:

```sh
systemctl --user stop potluck
podman volume export potluck-data > potluck-data.tar
systemctl --user start potluck
```

Restore only into an empty volume, and check that `potluck-data.tar` exists first, because the clearing step below deletes everything in the volume. Both `podman volume import` and `tar -x` merge into what the volume already holds, so a `server.db-wal` or `server.db-shm` left from the old data would be applied over the restored database, and a leftover repository would be mixed in with the restored ones. A clean stop removes the WAL; if a backup does carry `server.db-wal` or `server.db-shm` (the stop timed out), they belong with that `server.db` and are restored with it, as the whole-volume tarball does.

With podman:

```sh
podman stop potluck
podman run --rm -v potluck-data:/data debian:trixie-slim find /data -mindepth 1 -delete
podman volume import potluck-data potluck-data.tar
podman start potluck
```

With docker, unpack the archive into the emptied volume through a throwaway container:

```sh
docker stop potluck
docker run --rm -v potluck-data:/data debian:trixie-slim find /data -mindepth 1 -delete
docker run --rm -v potluck-data:/data -v "$PWD":/backup debian:trixie-slim tar -C /data -xf /backup/potluck-data.tar
docker start potluck
```

With the Quadlet unit:

```sh
systemctl --user stop potluck
podman run --rm -v potluck-data:/data debian:trixie-slim find /data -mindepth 1 -delete
podman volume import potluck-data potluck-data.tar
systemctl --user start potluck
```

## Verifying the image

Each release's notes give the image's digest (`sha256:...`). Pin that digest to know exactly what you run, with podman:

```sh
podman pull ghcr.io/phongguy/potluck@sha256:<digest from the release notes>
```

Every release's image carries a build provenance attestation. Check that this repository's release workflow built it with the GitHub CLI (`gh`), substituting the release tag:

```sh
gh attestation verify oci://ghcr.io/phongguy/potluck:<tag> --repo PhongGuy/potluck
```

The release's signed manifest, `potluck-release.json`, is attested the same way: download it from the release and run `gh attestation verify potluck-release.json --repo PhongGuy/potluck`.

## Bind mounts

To keep the data in a host directory instead of the named volume, the directory must belong to the container's user, uid 65532. Under rootless podman, give it that owner from your own account:

```sh
mkdir -p ~/potluck-data
podman unshare chown 65532:65532 ~/potluck-data
podman run -d --name potluck -p 127.0.0.1:8080:8080 -v ~/potluck-data:/data:Z -e POTLUCK_PUBLIC_URL=http://127.0.0.1:8080 ghcr.io/phongguy/potluck:latest
```

The `:Z` suffix relabels the directory for SELinux hosts (Fedora, RHEL); it is harmless elsewhere. With docker, use `docker run` with the same `-v` option, keeping `:Z` only on SELinux hosts, and give the directory its owner the way your docker runs. `docker info` lists `rootless` under Security Options when it runs rootless. With rootful docker, run `sudo chown 65532:65532 ~/potluck-data`. With rootless docker, uid 65532 in the container maps to a subordinate uid on the host, so set the owner from inside a throwaway container instead:

```sh
docker run --rm -v ~/potluck-data:/data debian:trixie-slim chown 65532:65532 /data
```

The data directory must be on a local filesystem that supports file locks. On NFS without lock support, `serve` stops with "cannot take the lock on .../server.lock".

## License

Licensed under either of MIT or Apache-2.0, at your option.

The container image also ships third-party software, including git under GPL-2.0; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
