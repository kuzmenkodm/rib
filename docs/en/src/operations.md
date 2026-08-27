# Operations

`rib` is designed primarily for non-interactive CI/CD use while retaining the
same interface locally. Production pipelines should pin the builder version,
platform, base-image digest, and network settings.

## Progress, stderr, and stdout

`--progress` accepts:

| Mode | Behavior |
|---|---|
| `auto` | Interactive UI when stderr is a terminal; line output otherwise |
| `color` | Forces the interactive UI |
| `plain` | Stable line-oriented CI messages |
| `quiet` | Hides progress while retaining the final summary |

Progress, warnings, and the summary go to stderr. When stdout is not a
terminal, a successful build writes only the manifest digest, without a prefix
or trailing newline:

```bash
IMAGE_DIGEST="$(rib build \
  --progress plain \
  --from scratch \
  --platform linux/amd64 \
  --copy ./server:/server,mode=0755 \
  --entrypoint /server \
  --to registry:registry.example.com/team/server:v1)"

cosign sign "registry.example.com/team/server@${IMAGE_DIGEST}"
```

Do not enable shell tracing around commands containing credentials, because
the shell may log raw arguments before `rib` starts.

## Concurrency

`--jobs` limits concurrent `COPY` builds, base-blob downloads, and target-blob
uploads. The default is the available CPU count capped at four. Increasing it
may improve network throughput while increasing CPU, disk, and registry load.
Reduce it under tight ephemeral-storage or bandwidth limits.

Destinations are processed sequentially in declaration order. Layer operations
within one registry destination are concurrent.

## Temporary data and base-layer cache

Layer temporary files use the system temporary directory (`TMPDIR` on Unix).
Registry and OCI outputs normally retain only compressed local layers. Docker
archive output also retains uncompressed layers. The archive's own temporary
file is created beside its final path.

For large images, provision storage for compressed local layers, downloaded
base layers, uncompressed Docker layers, and the temporary final archive.

Downloaded base blobs are not persistent by default. Enable caching with:

```bash
rib build ... --cache
rib build ... --cache --cache-path /cache/rib
```

The default path is `./.rib-cache`. Cached blobs are rehashed before reuse.
This cache stores downloaded base layers only; it neither stores built layers
nor replaces Cargo/npm caches. Do not let multiple `rib` processes write one
cache directory concurrently. A direct push to the same registry usually needs
no local cache because registry-side blob reuse avoids downloads.

## Network transport

Defaults are a 30-second connect timeout, a 60-second read timeout, three
attempts, and HTTPS with certificate verification. Configure these through
`--connect-timeout`, `--read-timeout`, and `--max-attempts`.

Source and target insecure options are separate:

- `--from-plain-http`, `--from-skip-tls`;
- `--to-plain-http`, `--to-skip-tls`.

The target options affect all registry destinations in the invocation. Use
them only for trusted development infrastructure.

## Multiple destinations

```bash
rib build \
  --from alpine:3.22 \
  --platform linux/amd64 \
  --copy ./server:/app/server,mode=0755 \
  --entrypoint /app/server \
  --to registry:registry.example.com/team/server:v1 \
  --to oci-archive:dist/server-oci.tar@server:v1 \
  --to docker-archive:dist/server-docker.tar@server:v1
```

The configuration and manifest are created once, so all outputs have one
manifest digest. They do not form a transaction: failure of a later output
does not roll back completed registry publications or archives.

## Logs and diagnostics

Enable detailed logs with:

```bash
RIB_LOG=rib=debug rib build --progress plain ...
```

`RUST_LOG` is intentionally not used because `cargo rib build` launches a user
Rust project that may interpret it. Use `plain`, `quiet`, or `auto` in CI;
`color` may emit terminal control sequences.

## Termination and partially published data

Ctrl-C cancels the top-level operation. Copy writers check cancellation during
writes; there is no dedicated SIGTERM handler in the current version.

Archives become visible through an atomic rename only after a complete write
and file synchronization. Registry manifests are sent after layers and the
configuration. Unreferenced blobs uploaded before an error can remain until
normal registry garbage collection.
