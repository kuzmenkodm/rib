# Registries and archive formats

Every output receives the same assembled `ImageBundle`; writers do not rebuild
the configuration or manifest. Registry, OCI archive, and Docker archive
outputs from one invocation therefore represent the same image and have the
same manifest digest.

## No preliminary base-image pull

The initial request fetches only the manifest, configuration, and descriptors.
Base layer bytes are downloaded only when:

- an OCI or Docker archive needs to be self-contained; or
- a target registry lacks a blob and cannot mount it from the source repository.

For archive output, unique base blobs are downloaded concurrently under the
`--jobs` limit before archive writing begins. For registry output, existing
blobs are detected by digest and reused; same-registry sources also attempt a
cross-repository mount. `scratch` has no base-layer download stage.

## OCI archive

An OCI archive is a tar file containing an OCI Image Layout:

```text
oci-layout
index.json
blobs/sha256/<manifest-hex>
blobs/sha256/<config-hex>
blobs/sha256/<layer-hex>
...
```

`index.json` contains one descriptor for the selected platform. A tag supplied
as `--to oci-archive:image.tar@app:v1` becomes the descriptor annotation
`org.opencontainers.image.ref.name`, which import tools use as the image tag.

Configuration, manifest, and compressed layers are stored at paths derived
from their digests. Duplicate digests are written once.

```bash
rib build ... \
  --to oci-archive:dist/image.tar@example/app:v1
```

## Docker archive

A Docker archive is compatible with `docker save` and `docker load`:

```text
manifest.json
<config-hex>.json
<diff-id-hex>/layer.tar
...
```

`manifest.json` contains optional `RepoTags` and an ordered layer list. Unlike
OCI archives, Docker archives store uncompressed layers and are usually much
larger. When such an output is requested, `rib` retains an uncompressed tar for
each new layer and decompresses gzip- or zstd-compressed base layers into
temporary files as needed.

```bash
rib build ... \
  --to docker-archive:dist/image.tar@example/app:v1
```

## Atomic archive writing

An archive is first written to a temporary file beside its destination. The
writer finishes the tar, flushes buffers, calls `fsync` on the file, and
atomically renames it to the target path. If an earlier step fails, an existing
destination remains unchanged. Keeping the temporary file beside the target is
required because atomic rename is guaranteed only within one filesystem.

## Registry publication

For each unique layer, `rib` uses the first applicable method:

```text
blob exists in the target repository?
    ├── yes ─► reuse it
    └── no
        ├── base layer from the same registry ─► attempt cross-repository mount
        │                                         ├── success
        │                                         └── failure ─┐
        └── built layer or another registry ──────────────────┴─► upload
```

Cross-repository mount adds a reference to a blob already stored elsewhere in
the same registry without transferring its bytes. Built layers are streamed
from local temporary storage. A required base layer is downloaded lazily and
then streamed to the target.

Layers upload concurrently under the `--jobs` limit. Configuration is uploaded
after layers, and the manifest is published last:

```text
layers ──► config ──► manifest
                       │
                       └── the tag moves at this point
```

Manifest publication is the commit point. Until then, an existing tag still
references its previous manifest; a failure cannot expose a partially uploaded
image under the new tag.

## Streaming and retries

Uploads use 4 MiB chunks and never load a complete layer into memory. A retry
reopens and retransmits the file from the beginning; resumable upload is not
implemented.

Timeouts, connection failures, HTTP `408`, `425`, `429`, `500`, and `502`–`599`
are retried up to the configured attempt count with exponential backoff and
jitter. Authentication and digest errors are not retried. Small configuration
and manifest JSON values remain in memory, while layer memory usage stays
bounded regardless of image size.

## Authentication and transport

Credential resolution for a registry is:

1. explicit `--credential <registry>=<user>:<password>`;
2. `auths` in the one selected Docker configuration;
3. anonymous access.

The Docker configuration is selected from `--docker-config`, then
`$DOCKER_CONFIG/config.json`, then `~/.docker/config.json`. No fallback to
other files occurs after selection. Docker Hub aliases are canonicalized to
`registry-1.docker.io`. `credsStore` and `credHelpers` are not executed.

Source and target registries use separate clients. `--from-plain-http` and
`--from-skip-tls` affect only the source role; corresponding `--to-*` options
affect every registry destination in that invocation. Use insecure transport
only for controlled local or development registries.
