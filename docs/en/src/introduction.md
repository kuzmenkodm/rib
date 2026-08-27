# OCI image model

An OCI image is a collection of content-addressed objects connected by
descriptors. `rib` creates those objects directly according to the OCI Image
Specification, without a Docker daemon or a temporary container.

This chapter defines the terms layer, digest, manifest, and configuration used
throughout the rest of this book.

## Analogy: an image as filesystem patches

Start with an empty filesystem. Each **layer** is a set of changes applied to
it: add a file, replace a file, or add a directory. Applying all layers in
order produces the container filesystem, much like applying a sequence of Git
commits produces a repository state.

`rib` does not implement every Dockerfile operation. It creates one kind of
layer: take files from the host and place them at selected image paths. Such a
layer is requested with `--copy`.

## Descriptors and digests

Every layer, configuration, and manifest is identified by a SHA-256 hash of
its own bytes. This hash is the object's **digest**. Changing one byte changes
the digest.

A **descriptor** is a small JSON value that specifies how to locate and verify
an object:

```json
{
  "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
  "digest": "sha256:def456...",
  "size": 28471122
}
```

The descriptor contains only the media type, size, and digest. The actual bytes
are stored separately—by digest in a registry or under `blobs/` in an OCI
archive. This is content-addressable storage: location is derived from content.
If two builds produce the same layer bytes, the registry sees the same digest
and does not need a second upload.

## Layer: a filesystem change

A layer is a tar archive describing a change relative to preceding layers.
Layers are applied in manifest order, so a later layer can replace a path from
an earlier one.

Every layer created from `--copy` is deterministic and gzip-compressed. It
contains only explicitly selected files and directories. `rib` executes no
container commands and computes no filesystem diff.

Multiple layers may be built concurrently, but `rib` restores command-line
order before assembling the image. Completion order therefore never changes
the image semantics or digest.

## Configuration: how the container runs

The image configuration is JSON containing runtime metadata rather than file
contents:

- `architecture`, `os`, and optional `variant` identify the target platform;
- `config.Entrypoint`, `config.Cmd`, and `config.Env` describe process startup;
- `config.WorkingDir` and `config.User` set the working directory and user;
- `config.ExposedPorts` and `config.Labels` declare ports and labels;
- `rootfs.diff_ids` lists uncompressed layer identifiers in application order;
- `history` records layer-producing and metadata-only operations;
- `created` records the image creation time.

For a registry base image, `rib` clones the base configuration, appends layer
and history data, sets `created`, and applies explicitly requested runtime
metadata such as `--entrypoint` and `--label`. Other fields, including the
base `Env`, are preserved; the current CLI does not modify `Env`.

For `scratch`, the configuration is created from nothing. The standalone CLI
uses `--platform` or its `linux/amd64` default; Cargo integration requires the
platform through metadata or the CLI. Layer and history lists start empty.
`scratch` is a special exact keyword, not an image stored in a registry.

## Manifest: the table of contents for one image

A manifest connects one configuration with an ordered layer list:

```json
{
  "schemaVersion": 2,
  "mediaType": "application/vnd.oci.image.manifest.v1+json",
  "config": {
    "mediaType": "application/vnd.oci.image.config.v1+json",
    "digest": "sha256:abc123...",
    "size": 1842
  },
  "layers": [
    {
      "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
      "digest": "sha256:def456...",
      "size": 28471122
    }
  ]
}
```

One manifest describes one platform-specific image. A registry tag is a
mutable reference to either a manifest digest or a multi-platform index. A
manifest digest is immutable because it is calculated from the manifest
bytes. For production, `registry.example.com/app@sha256:...` therefore
identifies one result more reliably than a tag.

## Image index: multiple platforms under one tag

An **image index** lists platform-specific manifest descriptors, allowing a
client to select the appropriate image for `linux/amd64`, `linux/arm64`, and
other platforms.

`rib` builds exactly one platform per invocation. When the base reference
resolves to an index, it selects the manifest matching `--platform`. The
output remains a platform-specific manifest, not a new multi-platform index.
An OCI archive contains an `index.json`, but it has a single reference to the
built manifest.

## Why layers have both `digest` and `diff_id`

Each compressed layer has two SHA-256 identifiers:

```text
diff_id = sha256(layer.tar)      — uncompressed content
digest  = sha256(layer.tar.gz)   — compressed bytes that are transferred
```

The same tar stream can be compressed by different tools or settings, changing
the gzip bytes and `digest` while leaving `diff_id` unchanged. Consequently:

- `digest` identifies the blob stored in registries and OCI archives and is
  written to `manifest.layers[i].digest`;
- `diff_id` identifies the unpacked result and is written to
  `config.rootfs.diff_ids[i]`.

Configurations and manifests are content-addressed in the same way:

```text
config_digest   = sha256(config.json bytes)
manifest_digest = sha256(manifest.json bytes)
```

Changing metadata such as a label changes the configuration bytes, which
changes its descriptor inside the manifest and therefore changes the final
manifest digest. The final digest fingerprints the complete image, including
metadata rather than filesystem contents alone.

## An image is an immutable graph

```text
tag ───────► manifest digest or image index digest
                  │
                  ├──► manifest ─► config descriptor ──► config JSON
                  │                    │
                  │                    └──► ordered layer descriptors
                  │                                      │
                  │                                      └──► compressed layer blobs
                  └──► image index ─► platform-specific manifest descriptors
```

Existing objects are never rewritten. A build creates a new graph root: `rib`
reuses base-layer descriptors, appends new layers, and creates a new
configuration and manifest while old blobs remain untouched.

The next chapters explain why this model is useful in rootless CI/CD and how
it is implemented inside `rib`.
