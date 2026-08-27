# Architecture

This chapter describes the internal components and the overall build pipeline.
The following chapters cover layer construction, OCI configuration and
manifests, and output formats in more detail.

The standalone `rib build` CLI and the `cargo rib build` integration converge
on the same pipeline:

```text
rib build   ─────────────┐
                         ├──► ImageBuildSpec ──► BuildRequest
cargo rib build ─► cargo ┘                         │
                                                   ▼
                         ┌────────────────────────────────────┐
                         │ base image + parallel COPY build   │
                         └─────────────────┬──────────────────┘
                                           ▼
                                      ImageBundle
                                           │
                         ┌─────────────────┼─────────────────┐
                         ▼                 ▼                 ▼
                    OCI archive      Docker archive       registry
```

Once an `ImageBuildSpec` exists, the request source no longer affects image
processing.

## Module responsibilities

| Module | Responsibility |
|---|---|
| `cli` | Parses standalone CLI options into a build specification |
| `cargo_integration` | Runs Cargo metadata and build, selects a workspace package, and merges metadata with CLI options |
| `app::build` | Validates the request and coordinates the complete build |
| `builder::copy` | Parses `--copy`, expands globs, and resolves image destinations |
| `builder::layer` | Creates a deterministic tar/gzip layer and its identifiers |
| `image` | Stores the platform, base image, and layer sources |
| `image::assemble` | Produces the final configuration, manifest, and `ImageBundle` |
| `registry` | Wraps `oci-client` for downloads, blob operations, manifest publication, and retries |
| `app::push` | Chooses blob reuse, cross-repository mount, or upload |
| `blob` | Lazily downloads and, for Docker archives, decompresses base layers |
| `cache` | Indexes and verifies persistent downloaded-base-layer cache entries |
| `archive` | Writes OCI and Docker archives |
| `auth` | Reads Docker configuration and explicit credentials, and canonicalizes registry names |
| `progress` | Provides terminal, CI-line, and quiet progress modes |

The `rib` and `cargo-rib` binaries delegate to `rib::run()` and
`rib::run_cargo()`, keeping lifecycle and error handling shared.

## Request normalization

Standalone options become `BuildArgs` and then `ImageBuildSpec`. Cargo
integration builds the same specification after merging
`[package.metadata.rib]` with CLI values. `BuildRequest::try_from` validates
the main invariants before network or filesystem work starts:

- `--jobs` is greater than zero;
- at least one `--to` destination is present;
- `--platform` matches `<os>/<arch>[/<variant>]`;
- every `--label` is a `key=value` pair;
- a port without a protocol receives `/tcp`;
- `--creation-time` is `epoch`, `now`, or valid RFC 3339;
- the exact word `scratch` denotes an empty base; everything else is parsed as
  an `oci_client::Reference`.

Downstream components therefore consume one validated request type regardless
of the entry point.

## Two independent initial branches

Fetching base metadata and creating local layers are independent and run
concurrently. The network branch is absent for `scratch`:

```text
registry base                            scratch
─────────────                            ───────
fetch manifest + config ──┐              create empty config ──┐
                           ├─► assemble                          ├─► assemble
build COPY layers ─────────┘              build COPY layers ────┘
```

Only the base manifest and configuration are downloaded at this point. Base
layer bytes are fetched later for a self-contained archive or when a target
registry cannot reuse a layer.

## Async and blocking boundaries

Registry access uses the asynchronous `oci-client` API. Filesystem traversal,
tar creation, and gzip compression are CPU- and disk-bound, so every `--copy`
runs in `tokio::task::spawn_blocking` rather than on Tokio worker threads.

A semaphore of size `--jobs` limits simultaneous layer builders. Separate
semaphores with the same numeric limit constrain later blob downloads and
uploads. Results carry their original indices and are reordered before
assembly, so completion timing cannot change manifest layer order.

## Assembly as the model boundary

Before assembly, the system holds base descriptors/configuration and local
`Layer` values with their digest, `diff_id`, and temporary storage. The
assembler combines them into:

```text
ImageBundle {
    config,
    config_bytes,
    config_digest,
    manifest_bytes,
    manifest_digest,
    layers: Vec<LayerSource>,
}
```

All registry and archive outputs consume the same bundle and ordered layer
list. Every destination in one invocation consequently has the same manifest
digest.

## Temporary-data lifecycle

A built layer owns an `Arc<LayerStorage>` for a temporary directory containing
`layer.tar.gz` and, only when a Docker archive is requested, `layer.tar`.
Cloning an image bundle shares these files rather than copying them.

Downloaded base layers are indexed by digest and reused across destinations in
one run. Without `--cache`, temporary data is removed when its last owner is
dropped. With `--cache`, base blobs remain under `.rib-cache` or
`--cache-path` and are verified by digest before reuse. Built `COPY` layers are
not cached between runs.

## Errors and cancellation

Errors at parsing, layer building, download, assembly, archive writing, and
registry publication boundaries receive contextual messages. If a `--copy`
worker fails, a shared cancellation token stops the remaining wrappers; the
coordinator waits for them and returns the original contextual error.

Blocking writers check cancellation between writes. Ctrl-C cancels the
top-level operation, although the current version has no separate SIGTERM
handler. Temporary directories are owned by RAII values and are cleaned when
work is cancelled or fails.

## The `oci-client` and `oci-spec` boundary

`oci-client` is used only for registry transport: authentication, manifest
resolution, blob checks, mounts, uploads, and downloads. `oci-spec` is the
canonical in-memory model for platform, descriptors, configuration, and
manifest assembly. Adapter code converts registry values once at the boundary,
so archive generation and assembly do not depend on registry transport types.
