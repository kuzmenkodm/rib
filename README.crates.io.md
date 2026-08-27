# rib — Rust Image Builder

`rib` is a rootless OCI image builder for packaging pre-built application
artifacts. It creates image layers with ordinary filesystem operations and does
not require a Docker daemon, a privileged container, or commands executed
inside the image being assembled.

> **CLI-only package:** `rust-image-builder` is distributed as an application,
> not as a Rust library. The `rib` library target exists only to share internal
> implementation between the `rib` and `cargo-rib` binaries. Library usage is
> unsupported, and its Rust API has no stability guarantees.

## Why rib

- Builds OCI images without root privileges or a container runtime.
- Avoids a mandatory full pull of the base image when publishing to a registry.
- Reuses existing base layers through registry blob checks and
  cross-repository mounts.
- Produces deterministic `COPY` layers from explicitly selected files.
- Writes the final manifest digest to stdout for signing with tools such as
  `cosign`.
- Supports registry, OCI archive, and Docker archive outputs.
- Provides a Cargo integration through `cargo rib build`.

## Installation

Rust 1.88 or newer is required:

```bash
cargo install rust-image-builder --locked
```

The package installs two executables:

- `rib` — standalone image builder;
- `cargo-rib` — Cargo subcommand invoked as `cargo rib`.

## Standalone example

The following command packages a statically linked executable into an OCI
archive based on `scratch`:

```bash
rib build \
  --from scratch \
  --platform linux/amd64 \
  --copy ./server:/server,mode=0755 \
  --entrypoint /server \
  --to oci-archive:dist/server.tar@server:v1
```

To publish directly to a registry, use a `registry:` output:

```bash
IMAGE_DIGEST="$(rib build \
  --from alpine:3.22 \
  --platform linux/amd64 \
  --copy ./server:/usr/local/bin/server,mode=0755 \
  --entrypoint /usr/local/bin/server \
  --to registry:registry.example.com/team/server:v1)"

cosign sign "registry.example.com/team/server@${IMAGE_DIGEST}"
```

Progress and diagnostics are written to stderr. When stdout is redirected, a
successful build writes only the resulting manifest digest to stdout.

## Cargo integration

Image settings for a Rust package can be stored in `Cargo.toml`:

```toml
[package.metadata.rib]
artifact = "target/x86_64-unknown-linux-musl/release/server"
platform = "linux/amd64"
from = "scratch"
to = ["oci-archive:dist/server.tar@server:v1"]
destination = "/server"
entrypoint = "/server"
cargo-args = [
  "--release",
  "--locked",
  "--target", "x86_64-unknown-linux-musl",
]
```

The configured artifact is built and packaged with:

```bash
cargo rib build
```

`rib` does not install compilation targets or perform cross-compilation
automatically. The required Rust target and linker must already be available in
the build environment.

## Documentation

- [Russian documentation](https://kuzmenkodm.github.io/rib/ru/)
- [English documentation](https://kuzmenkodm.github.io/rib/en/)
- [Source repository](https://github.com/kuzmenkodm/rib)

## Scope

`rib` packages files that have already been built. It does not process
Dockerfiles, run commands inside the image, install packages, create
multi-platform indexes, or provide a stable Rust library API.

## License

Licensed under the MIT License.
