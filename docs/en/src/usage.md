# Getting started

## Installation

### crates.io

Installing the published crate requires Rust 1.88 or later:

```bash
cargo install rust-image-builder --version 0.1.0 --locked
```

Omit `--version` to install the latest published version:

```bash
cargo install rust-image-builder --locked
```

Package page: [rust-image-builder 0.1.0 on crates.io](https://crates.io/crates/rust-image-builder/0.1.0).

### GitHub Releases

Prebuilt binaries do not require a Rust installation. Archives and checksum
files are available from [GitHub release v0.1.0](https://github.com/kuzmenkodm/rib/releases/tag/v0.1.0).

Example for Linux x86_64 with glibc:

```bash
version=v0.1.0
archive="rib-${version}-x86_64-unknown-linux-gnu.tar.gz"
base_url="https://github.com/kuzmenkodm/rib/releases/download/${version}"

curl -LO "${base_url}/${archive}"
curl -LO "${base_url}/${archive}.sha256"
sha256sum --check "${archive}.sha256"
tar -xzf "${archive}"
mkdir -p "$HOME/.local/bin"
install -m 0755 rib cargo-rib "$HOME/.local/bin/"
```

Ensure `$HOME/.local/bin` is in `PATH`. On Windows, download
`rib-v0.1.0-x86_64-pc-windows-msvc.zip`, verify it against the accompanying
`.sha256` file, extract `rib.exe` and `cargo-rib.exe`, and add their directory
to `PATH`.

### From source

```bash
git clone https://github.com/kuzmenkodm/rib.git
cd rib
cargo install --path . --locked
```

All installation methods provide two binaries:

- `rib` — the standalone CLI for packaging existing files;
- `cargo-rib` — the Cargo subcommand invoked as `cargo rib build`.

Display the current command-line reference with:

```bash
rib build --help
cargo rib build --help
```

## Building from a registry image

The following command adds a prebuilt executable to an Alpine base image and
publishes the result directly to a registry:

```bash
IMAGE_DIGEST="$(rib build \
  --from alpine:3.22 \
  --platform linux/amd64 \
  --copy ./server:/app/server,mode=0755 \
  --entrypoint /app/server \
  --to registry:registry.example.com/team/server:v1)"
```

`rib` fetches the base manifest and configuration but does not perform a full
preliminary pull. Inherited layers are reused by digest in the registry.
`IMAGE_DIGEST` receives the digest of the published manifest.

## Building from scratch

Use an empty base for a statically linked executable:

```bash
rib build \
  --from scratch \
  --platform linux/amd64 \
  --copy ./server:/server,mode=0755,chown=65532:65532 \
  --entrypoint /server \
  --user 65532:65532 \
  --to oci-archive:dist/server.tar@server:v1
```

`scratch` is a special marker for an empty base and must be written exactly in
that form. `scratch:latest` is treated as an ordinary image reference that
`rib` will attempt to fetch from a registry.

`scratch` contains no dynamic loader, system libraries, CA certificates, or
shell. Include every runtime dependency in the artifact or add it through
another `--copy` option.

## The `--copy` format

```text
<source>:<destination>[,mode=<octal>][,chown=<uid>[:<gid>]]
```

Examples:

```bash
--copy ./server:/usr/local/bin/server,mode=0755
--copy ./config:/etc/server/,chown=65532:65532
--copy 'target/release/*.so:/usr/local/lib/'
```

Each `--copy` creates a separate layer, and layer order follows option order.
Destination syntax resembles Dockerfile `COPY`, but only the subset described
in this book is implemented. Symbolic links are unsupported, and a destination
for multiple glob matches must end in `/`. Quote glob patterns so `rib`, not
the shell, expands them.

## Output destinations

`--to` can be supplied more than once:

```text
registry:<image-reference>
oci-archive:<path>[@<tag>]
docker-archive:<path>[@<tag>]
```

Example:

```bash
rib build ... \
  --to registry:registry.example.com/team/server:v1 \
  --to oci-archive:dist/server-oci.tar@server:v1 \
  --to docker-archive:dist/server-docker.tar@server:v1
```

Destinations are processed sequentially and do not form one transaction.

## Authentication

Supply an explicit credential as:

```bash
--credential 'registry.example.com=user:password'
```

Alternatively, `rib` reads the `auths` section from one Docker configuration.
It selects an explicitly supplied `--docker-config`, then
`$DOCKER_CONFIG/config.json`, then `~/.docker/config.json`. It does not search
other files after selecting a configuration. An explicit `--credential`
overrides the selected Docker configuration.
