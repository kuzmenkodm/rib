# Cargo integration

The package installs `cargo-rib`, which Cargo invokes as:

```bash
cargo rib build
```

The integration converts Cargo metadata and CLI arguments into an
`ImageBuildSpec`, then uses the same image pipeline as `rib build`.

## Operation sequence

`cargo rib build`:

1. runs `cargo metadata --format-version 1 --no-deps`;
2. selects a workspace package;
3. reads `[package.metadata.rib]` from its `Cargo.toml`;
4. merges metadata and CLI arguments;
5. runs `cargo build --manifest-path <path>` with the configured Cargo args;
6. verifies that `artifact` exists and is a regular file;
7. adds it as the final `COPY` layer with mode `0755`;
8. assembles and publishes the image through the common pipeline.

Artifact path, platform, and base image are explicit. The integration does not
infer a binary target, architecture, or linkage type.

## Configuration location and format

Configuration belongs in `[package.metadata.rib]` for the selected package.
Field names use kebab-case, and unknown fields are errors. After metadata and
CLI merging, `artifact`, `platform`, `from`, and at least one `to` destination
are required. Any of them may come from either source; `to` is not restricted
to CI and is a normal metadata field.

## Artifact and Cargo fields

| Field | Type and default | Description and example |
|---|---|---|
| `artifact` | String; required after merging | Regular file produced by Cargo, for example `"target/x86_64-unknown-linux-musl/release/taskd"`. Metadata paths are relative to `Cargo.toml`. |
| `cargo-args` | String array; `[]` | Arguments after `cargo build --manifest-path <path>`, for example `["--release", "--locked", "--target", "x86_64-unknown-linux-musl", "--bin", "taskd"]`. |
| `destination` | String; `/app/<artifact name>` | Absolute artifact path in the image, for example `"/usr/local/bin/taskd"`. It cannot end in `/`; the artifact is the final layer with mode `0755`. |
| `copies` | String array; `[]` | Extra layers such as `["config/default.toml:/etc/taskd/config.toml", "assets:/opt/taskd/assets/"]`, inserted before the artifact. Metadata sources are relative to `Cargo.toml`. |

Each `copies` item uses:

```text
<source>:<destination>[,mode=<octal>][,chown=<uid>[:<gid>]]
```

```toml
copies = [
  "config/default.toml:/etc/taskd/config.toml,mode=0644,chown=65532:65532",
  "assets:/opt/taskd/assets/",
]
```

## Base, platform, and destinations

| Field | Type and default | Description and example |
|---|---|---|
| `platform` | String; required after merging | `<os>/<arch>[/<variant>]`, such as `"linux/amd64"` or `"linux/arm64/v8"`. The artifact is not inspected for compatibility. |
| `from` | String; required after merging | A base such as `"alpine:3.22"`, a digest-qualified reference, or exact `"scratch"`. |
| `to` | String array; `[]`, at least one after merging | Registry, OCI archive, or Docker archive destinations. Multiple values are allowed. |

Relative archive paths in metadata are resolved from the `Cargo.toml`
directory. CLI destinations are appended after metadata destinations:

```toml
[package.metadata.rib]
to = ["oci-archive:dist/taskd.tar@taskd:local"]
```

```bash
cargo rib build --to registry:registry.example.com/team/taskd:v1
```

This creates both outputs. There is no CLI replacement mode for `to`; omit the
metadata field when a CI job should publish only to its dynamic destination.

## Container runtime configuration

| Field | Type and default | Description and example |
|---|---|---|
| `entrypoint` | String or string array; `[destination]` | OCI Entrypoint, such as `"/taskd --log-format json"` or `["/taskd", "--log-format", "json"]`. Replacing it clears inherited Cmd unless `cmd` or `keep-cmd = true` is set. |
| `cmd` | String or string array; not explicitly set | OCI Cmd, such as `["serve", "--port", "8080"]`; replaces inherited Cmd. |
| `labels` | String array; `[]` | `key=value` labels merged with base labels; duplicate keys are overwritten. |
| `ports` | String array; `[]` | Ports such as `["8080", "8443/tcp", "8125/udp"]`; omitted protocols become `/tcp`. |
| `workdir` | String; inherited | Process working directory, for example `"/var/lib/taskd"`. |
| `user` | String; inherited | OCI user such as `"65532:65532"`, `"1000"`, or `"app"`; existence is not checked in rootfs. |
| `keep-cmd` | Boolean; `false` | Retains base Cmd when Entrypoint changes; does not override explicit `cmd`. |
| `creation-time` | String; `"epoch"` | `"epoch"`, `"now"`, or RFC 3339 such as `"2026-08-27T12:00:00Z"`. Use epoch for reproducibility. |

String `entrypoint` and `cmd` values use shell quoting rules. Arrays specify
argv without further parsing and are preferable when argument boundaries must
be exact.

## Concurrency and cache

| Field | Type and default | Description and example |
|---|---|---|
| `jobs` | Positive integer; available CPUs capped at `4` | Maximum simultaneous layer builds and blob transfers. |
| `cache` | Boolean; `false` | Enables persistent caching of downloaded base layers, not built layers. |
| `cache-path` | String; `".rib-cache"` when enabled | Cache directory such as `"/cache/rib"`. This field alone does not enable caching. Relative paths use the process working directory. |

## Registry client fields

| Field | Type and default | Description |
|---|---|---|
| `connect-timeout` | Positive integer; `30` | Connection timeout in seconds. |
| `read-timeout` | Positive integer; `60` | Registry read timeout in seconds. |
| `max-attempts` | Positive integer; `3` | Maximum attempts for each registry operation. |
| `from-plain-http` | Boolean; `false` | HTTP for the source registry; development use only. |
| `from-skip-tls` | Boolean; `false` | Disables source TLS certificate verification; not for production. |
| `to-plain-http` | Boolean; `false` | HTTP for every registry destination; development use only. |
| `to-skip-tls` | Boolean; `false` | Disables certificate verification for every registry destination; not for production. |

Credentials, Docker configuration, and progress mode are intentionally absent
from metadata. Supply them through `--credential`, `--docker-config`, and
`--progress` or the environment; do not put secrets in `Cargo.toml`.

## Metadata and CLI merging

- CLI scalar values replace metadata values.
- Booleans use logical OR: CLI can enable an option but cannot disable a
  metadata value of `true`.
- `to`, `copies`, `labels`, `ports`, and Cargo arguments are concatenated with
  metadata values first.
- The artifact is always the final layer.
- Artifact and copy sources from metadata are relative to `Cargo.toml`; CLI
  sources are relative to the current working directory.

Cargo arguments from the CLI follow `--` and are appended after `cargo-args`:

```bash
cargo rib build -- --release --locked --bin taskd
```

## Complete configuration example

This example includes every supported metadata field:

```toml
[package]
name = "taskd"
version = "0.1.0"
edition = "2021"
rust-version = "1.94"

[package.metadata.rib]
artifact = "target/x86_64-unknown-linux-musl/release/taskd"
cargo-args = [
  "--release",
  "--locked",
  "--target", "x86_64-unknown-linux-musl",
  "--bin", "taskd",
]
destination = "/taskd"
copies = [
  "config/default.toml:/etc/taskd/config.toml,mode=0644,chown=65532:65532",
  "assets:/opt/taskd/assets/",
]

platform = "linux/amd64"
from = "scratch"
to = [
  "registry:registry.example.com/team/taskd:v1",
  "oci-archive:dist/taskd.tar@taskd:v1",
]

entrypoint = ["/taskd"]
cmd = ["serve", "--port", "8080"]
labels = [
  "org.opencontainers.image.title=taskd",
  "org.opencontainers.image.version=0.1.0",
]
ports = ["8080/tcp"]
workdir = "/"
user = "65532:65532"
keep-cmd = false
creation-time = "epoch"

jobs = 4
cache = true
cache-path = ".rib-cache"

connect-timeout = 30
read-timeout = 60
max-attempts = 3
from-plain-http = false
from-skip-tls = false
to-plain-http = false
to-skip-tls = false
```

With `to` configured, no CLI destination is required:

```bash
cargo rib build
```

Credentials remain external:

```bash
cargo rib build \
  --credential "registry.example.com=${REGISTRY_USER}:${REGISTRY_PASSWORD}"
```

For a CI-generated tag, omit metadata `to` and pass it dynamically:

```bash
cargo rib build \
  --credential "$CI_REGISTRY=$CI_REGISTRY_USER:$CI_REGISTRY_PASSWORD" \
  --to "registry:$CI_REGISTRY_IMAGE/taskd:$CI_COMMIT_SHORT_SHA"
```

This is a configuration choice, not a limitation of Cargo integration.

## Configuration without metadata

Every required value can be supplied through CLI options:

```bash
cargo rib build \
  --artifact target/x86_64-unknown-linux-musl/release/taskd \
  --platform linux/amd64 \
  --from scratch \
  --to registry:registry.example.com/team/taskd:v1 \
  --destination /taskd \
  --copy config/default.toml:/etc/taskd/config.toml \
  --entrypoint /taskd \
  -- \
  --release --locked --target x86_64-unknown-linux-musl --bin taskd
```

## Workspace package selection

When the current directory is within a Cargo package, that package is chosen.
From a virtual workspace root, selection succeeds only with one
`workspace.default-member` or a single package in the workspace. For an
ambiguous workspace, run `cargo rib build` from the required package directory.

## Integration limitations

`cargo rib build` does not detect artifact architecture or linkage, add dynamic
loaders or shared libraries, install Rust targets, cross-compile, or discover
certificates and other runtime files. With `from = "scratch"`, include all
dependencies in the artifact or list them in `copies`.
