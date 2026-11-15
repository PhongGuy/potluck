# Third-party notices

Potluck itself is licensed under MIT OR Apache-2.0 and links no GPL code.

The container image `ghcr.io/phongguy/potluck` is built on `debian:trixie-slim` and adds these Debian packages, unmodified, from Debian trixie, together with the packages they depend on:

## git and its dependencies

git is licensed under GPL-2.0-only; each library it pulls in carries the licence recorded in its `/usr/share/doc/<package>/copyright` file inside the image. Potluck runs the `git` binary as a separate process and does not link it.

The corresponding source is available from Debian:

- the package pool: https://deb.debian.org/debian/pool/main/g/git/
- on a Debian trixie system with source repositories enabled: `apt-get source git`

To read the exact version shipped in an image, with podman (docker takes the same arguments):

```sh
podman run --rm --entrypoint dpkg-query ghcr.io/phongguy/potluck:latest -W git
```

The pool keeps only current versions; the source of that exact version stays available at `https://snapshot.debian.org/package/git/<version>/`.

## ca-certificates

Licensed under MPL-2.0 and others, as listed in Debian's copyright file for the package (`/usr/share/doc/ca-certificates/copyright` in the image). Its source is in the same pool, under https://deb.debian.org/debian/pool/main/c/ca-certificates/.

## Debian base system

The packages of the `debian:trixie-slim` base carry their own licences, recorded in `/usr/share/doc/*/copyright` inside the image.
