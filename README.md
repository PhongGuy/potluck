# Potluck

A self-hosted team server for sharing workspaces of local-first desktop apps, git-style. Its first clients are Apinata and Dbento. It runs as one container under podman or docker.

Work in progress.

## Run it

The image is `ghcr.io/phongguy/potluck:latest`, built for amd64 and arm64. It runs as a non-root user (uid 65532) and keeps everything it stores in one volume mounted at `/data`.

The package is private until the project is made public, so for now log in to the registry once before the first pull, with a GitHub personal access token that holds the `read:packages` scope. The command asks for your GitHub user name and, as the password, the token.

With podman:

```sh
podman login ghcr.io
```

With docker:

```sh
docker login ghcr.io
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

The admin then signs in from the app, or with any HTTP client: `POST /api/v1/session` with `{"username", "password", "token_name"}` answers a token. `potluck admin --help` lists the rest: users, tokens, teams, workspaces, grants and overrides; `--json` prints the answer as JSON instead of a table.

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

The server trusts `X-Forwarded-For` only from the addresses listed in `POTLUCK_TRUSTED_PROXIES` (default: none; the setting lands with the server's accounts features), so once it exists, put your proxy's address there.

## Updating

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

## Backups

Everything lives in the `potluck-data` volume: the database `/data/server.db` (with its `server.db-wal` and `server.db-shm` files) and the workspaces' git repositories under `/data/repos`. The one-liners, `compose.yaml` and the Quadlet unit all use that same volume name. Stop the server so the copy is consistent, export the volume, then start it again.

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

To restore with podman, stop the server and run `podman volume import potluck-data potluck-data.tar`. With docker, stop it and unpack the archive into the volume through a throwaway container:

```sh
docker stop potluck
docker run --rm -v potluck-data:/data -v "$PWD":/backup debian:trixie-slim tar -C /data -xf /backup/potluck-data.tar
docker start potluck
```

## Verifying the image

Each release's notes give the image's digest (`sha256:...`). Pin that digest to know exactly what you run, with podman:

```sh
podman pull ghcr.io/phongguy/potluck@sha256:<digest from the release notes>
```

Build provenance attestations are available once the repository is public. From then on, check an image with the GitHub CLI (`gh`), substituting the release tag:

```sh
gh attestation verify oci://ghcr.io/phongguy/potluck:<tag> --repo PhongGuy/potluck
```

## Bind mounts

To keep the data in a host directory instead of the named volume, the directory must belong to the container's user, uid 65532. Under rootless podman, give it that owner from your own account:

```sh
mkdir -p ~/potluck-data
podman unshare chown 65532:65532 ~/potluck-data
podman run -d --name potluck -p 127.0.0.1:8080:8080 -v ~/potluck-data:/data:Z -e POTLUCK_PUBLIC_URL=http://127.0.0.1:8080 ghcr.io/phongguy/potluck:latest
```

The `:Z` suffix relabels the directory for SELinux hosts (Fedora, RHEL); it is harmless elsewhere. With docker, run `sudo chown 65532:65532 ~/potluck-data` instead and use `docker run` with the same `-v` option, keeping `:Z` only on SELinux hosts.

## License

Licensed under either of MIT or Apache-2.0, at your option.

The container image also ships third-party software, including git under GPL-2.0; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
