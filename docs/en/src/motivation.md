# Purpose and design principles

`rib` handles the final stage of a build pipeline. Once an application has
been compiled and tested, it packages the artifact and required runtime files
as an image and publishes it to a registry or writes it to an archive.

```text
source code
    │
    ▼
compile and test
    │
    ▼
artifact + runtime files
    │
    ▼
rib
    ├──► registry
    ├──► OCI archive
    └──► Docker archive
```

For Rust projects, `cargo rib build` first runs `cargo build` and then passes
the resulting artifact into the common image pipeline. The standalone
`rib build` command does not build the application itself.

## Rootless builds through file operations

`rib` never starts a process inside the image being assembled. It creates new
layers exclusively from files selected by `--copy` or
`[package.metadata.rib]`. It only needs permission to read inputs, write
temporary tar/gzip files, access the OCI Distribution API, and write a local
archive when requested.

No Docker daemon, container runtime, overlay filesystem, mount namespace,
Docker socket, or build container is required. Ownership and permissions in
the resulting image are written directly into tar headers, so image-level
`chown` does not require equivalent host privileges. This makes Docker and
Kubernetes builds ordinary unprivileged workloads.

## Working with a base image

Initially, `rib` fetches only the base manifest, configuration, and layer
descriptors. It does not perform the equivalent of a full `docker pull`.

When publishing to a registry, OCI content addressing is used:

1. a blob already present in the target repository is not uploaded again;
2. repositories on the same registry use cross-repository mount when allowed;
3. only a missing blob is downloaded from the source and uploaded to the
   target.

The registry remains both the source of truth and the store of reusable
layers. OCI and Docker archives must contain every base layer and therefore
download all of them. A registry output downloads only blobs that the target
could not reuse.

## Caching the build environment

CI jobs commonly run in Docker containers or Kubernetes pods. `rib` can be
included directly in the build-environment image—for example, in a Node.js
Alpine image containing the frontend toolchain. The runner's container runtime
caches this image normally, so later jobs start in an environment where the OS
dependencies, Node.js, and `rib` are already available.

Application dependency caches such as `node_modules`, the Cargo registry, and
`target` remain normal CI caches. `rib build --cache` stores only downloaded
base-image blobs; it does not cache newly built `COPY` layers between runs.

## Composition with signing systems

`rib` deliberately delegates signing to tools such as `cosign`. Human-readable
progress and the final summary go to stderr. When stdout is redirected or used
in command substitution, stdout contains only the manifest digest:

```bash
IMAGE_DIGEST="$(rib build ... --to registry:registry.example.com/team/app:v1)"
cosign sign "registry.example.com/team/app@${IMAGE_DIGEST}"
```

The script does not parse logs. The immutable digest can be passed directly to
signing, attestation, or deployment tooling.

## Deliberate boundaries

`rib` does not process Dockerfiles or execute arbitrary image commands. It
does not support `RUN`, install packages while packaging, perform cross-
compilation or emulation, infer Cargo artifacts or linkage, build a multi-
platform index in one invocation, sign images, or generate SBOM/provenance.

Use a full Dockerfile-compatible builder when the final root filesystem must
be modified through `RUN`. `rib` is intended for prepared files that need a
rootless, reproducible, automation-friendly packaging stage.
