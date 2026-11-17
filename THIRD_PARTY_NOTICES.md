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

## tini

Licensed under Expat (the MIT licence), as listed in Debian's copyright file for the package (`/usr/share/doc/tini/copyright` in the image). Its source is in the same pool, under https://deb.debian.org/debian/pool/main/t/tini/.

## Debian base system

The packages of the `debian:trixie-slim` base carry their own licences, recorded in `/usr/share/doc/*/copyright` inside the image.

## aws-lc

The `potluck` binary statically links AWS-LC, the cryptography library behind its HTTPS update check: rustls uses it through the `aws-lc-rs` crate (ISC AND (Apache-2.0 OR ISC)), which builds it from the `aws-lc-sys` crate (ISC AND (Apache-2.0 OR ISC) AND Apache-2.0 AND MIT AND BSD-3-Clause AND (Apache-2.0 OR ISC OR MIT) AND (Apache-2.0 OR ISC OR MIT-0)). The full statement is the `LICENSE` file of each crate; the Apache License 2.0 text is the same as this repository's `LICENSE-APACHE`. The code compiled into the binary carries these notices:

- Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved. (AWS-LC, aws-lc-rs and s2n-bignum: Apache-2.0 OR ISC, and Apache-2.0 OR ISC OR MIT-0)
- Copyright (c) 2014-2024 Google Inc. (ISC); Copyright (c) 2025-2026 Google Inc. (Apache-2.0); contributions by Brian Smith (2016), Robert Nagy (2022) and Arm Ltd (2020) (ISC). From BoringSSL.
- Copyright (c) 1998-2011 The OpenSSL Project. All rights reserved. Copyright (c) 1995-1998 Eric Young (eay@cryptsoft.com). All rights reserved. Contributions by Sun Microsystems, Inc. (2002), Nokia (2005) and Intel Corporation (2012-2021). All Apache-2.0. From OpenSSL.
- Copyright (c) The mlkem-native project authors; Copyright (c) The mldsa-native project authors (Apache-2.0 OR ISC OR MIT).
- Copyright (c) 2015-2020 the fiat-crypto authors (MIT).
- Copyright (C) 2017 - 2025, Stephan Mueller <smueller@chronox.de> (Jitter Entropy, which AWS-LC distributes under BSD-3-Clause and not under GPL-2.0).
- The Keccak and AES reference implementations are public domain (CC0).

ISC License:

```
Permission to use, copy, modify, and/or distribute this software for
any purpose with or without fee is hereby granted, provided that the
above copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL
WARRANTIES WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED
WARRANTIES OF MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE
AUTHOR BE LIABLE FOR ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL
DAMAGES OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR
PROFITS, WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER
TORTIOUS ACTION, ARISING OUT OF OR IN CONNECTION WITH THE USE OR
PERFORMANCE OF THIS SOFTWARE.
```

MIT License:

```
Permission is hereby granted, free of charge, to any person obtaining
a copy of this software and associated documentation files (the
"Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish,
distribute, sublicense, and/or sell copies of the Software, and to
permit persons to whom the Software is furnished to do so, subject to
the following conditions:

The above copyright notice and this permission notice shall be
included in all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND,
EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
```

BSD 3-Clause License:

```
Redistribution and use in source and binary forms, with or without
modification, are permitted provided that the following conditions
are met:

1. Redistributions of source code must retain the above copyright
   notice, this list of conditions and the following disclaimer.

2. Redistributions in binary form must reproduce the above copyright
   notice, this list of conditions and the following disclaimer in the
   documentation and/or other materials provided with the distribution.

3. Neither the name of the copyright holder nor the names of its
   contributors may be used to endorse or promote products derived
   from this software without specific prior written permission.

THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS
"AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT
LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR
A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT
HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL,
SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE,
DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY
THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
(INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
```
