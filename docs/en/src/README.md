# rib — Rust Image Builder

**rib** is a rootless OCI image builder written in Rust. Its name references
the project that inspired it, [Jib](https://github.com/GoogleContainerTools/jib).
It packages prebuilt artifacts and runtime files without a Docker daemon,
privileged containers, or commands executed inside the image being built.

`rib` creates reproducible `COPY` layers using file operations and publishes
the result directly to a registry, an OCI archive, or a Docker archive.

## When to use rib

Use `rib` when an application has already been built and you need to:

- package a binary, a frontend, or another set of files as an OCI image;
- build the image without root access or a container runtime;
- integrate image packaging and signing into a CI/CD pipeline.

`rib` does not process Dockerfiles. If image preparation requires `RUN` or
other Dockerfile-specific instructions, use a compatible builder such as
[BuildKit](https://github.com/moby/buildkit) or
[Buildah](https://github.com/containers/buildah).

`rib` can be installed directly in a build-environment image so Docker or
Kubernetes can cache that image normally. Repeated builds do not require a
mandatory full pull of the base image: its root filesystem is not unpacked
unless the selected output requires it.

## How to navigate this book

| Goal | Chapter |
|---|---|
| Install `rib` and build the first image | [Getting started](usage.md) |
| Understand the project's scope | [Purpose and design principles](motivation.md) |
| Learn about manifests, configs, layers, and digests | [OCI image model](introduction.md) |
| Configure a Rust project | [Cargo integration](architecture/cargo.md) |
| Add image building and signing to GitLab CI | [CI/CD, Docker, and Kubernetes](operations/ci-kubernetes.md) |
| Configure caching, logs, timeouts, and concurrency | [Operations](operations.md) |
| Review security properties | [Reproducibility and security](operations/reproducibility-security.md) |
| Study rib internals | [Architecture](architecture.md) |
| Check whether rib fits a particular use case | [Limitations](limitations.md) |

For a first look, read [Purpose and design principles](motivation.md) and
[Getting started](usage.md). The remaining chapters can be used as reference
material when configuring a particular build scenario.
