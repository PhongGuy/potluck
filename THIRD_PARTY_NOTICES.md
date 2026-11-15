# Third-party notices

Potluck itself is licensed under MIT OR Apache-2.0 and links no GPL code.

The container image `ghcr.io/phongguy/potluck` is built on `debian:trixie-slim` and adds these Debian packages, unmodified, from Debian trixie:

## git

Licensed under GPL-2.0-only. Potluck runs the `git` binary as a separate process and does not link it.

The corresponding source is available from Debian:

- the package pool: https://deb.debian.org/debian/pool/main/g/git/
- on a Debian trixie system with source repositories enabled: `apt-get source git`

To read the exact version shipped in an image, with podman (docker takes the same arguments):

```sh
podman run --rm --entrypoint dpkg-query ghcr.io/phongguy/potluck:latest -W git
```

## ca-certificates

Licensed under MPL-2.0 and others, as listed in Debian's copyright file for the package (`/usr/share/doc/ca-certificates/copyright` in the image). Its source is in the same pool, under https://deb.debian.org/debian/pool/main/c/ca-certificates/.

## Debian base system

The packages of the `debian:trixie-slim` base carry their own licences, recorded in `/usr/share/doc/*/copyright` inside the image.
