# Limitations

## Image construction

- Only file addition through `COPY` and runtime metadata changes are supported;
  there is no `RUN` or Dockerfile parser.
- Package installation and whiteout layers for removing base files are absent.
- Symbolic links and special source files are rejected.
- Absolute Windows source paths containing `:` cannot be used in `--copy`; use
  relative paths.
- One invocation creates one platform-specific manifest. Build a multi-platform
  index with another tool or pipeline stage.
- `scratch` contains no libc, CA certificates, shell, or runtime files.

## Caching

- Without `--cache`, downloaded base blobs are removed after the process exits.
- The cache stores only downloaded base layers, not built `COPY` layers.
- One cache directory is not safe for concurrent writers.
- Registry-side blob checks and cross-repository mounts normally make a local
  cache unnecessary for repeated pushes to the same registry.

## Cargo integration

- Artifact, platform, and base image are explicit.
- Cross-compilation, target installation, and binary-target selection are not
  automatic.
- Artifact architecture and linkage are not inspected.
- In an ambiguous workspace, run the command from the required package.

## Registries and publication

- Multiple destinations are sequential and non-transactional.
- A failure in a later destination does not roll back earlier results.
- Blob retries restart from the beginning; resumable upload is absent.
- Docker credential helpers are not executed; only explicit credentials and
  the `auths` section are supported.
- Signing, SBOM, and provenance are external; stdout exposes the manifest
  digest for tools such as `cosign`.
- Ctrl-C uses cooperative cancellation; there is no separate SIGTERM handler.

## Possible future work

- caching built layers with safe concurrent access;
- multi-platform index generation;
- registry-native referrers for SBOM and provenance;
- resumable uploads;
- external credential providers and cloud workload identity;
- coordination across multiple publication destinations.
