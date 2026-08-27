# Image configuration, manifest, and `scratch`

Assembly combines the base image, new layers, and runtime metadata into one
immutable `ImageBundle`. It is a CPU-only step over `oci-spec` values; it does
not access the network or filesystem.

## Base-image representation

```text
Image {
    source: Option<ImageSource>,
    config: ImageConfiguration,
    layers: Vec<Descriptor>,
}
```

`Some(ImageSource)` records a real registry reference and its manifest digest.
`None` represents `scratch`. An empty base intentionally has no fabricated
registry identity that could leak into annotations or publication logic.

## Registry base images

For a normal image reference, the registry adapter:

1. selects credentials;
2. fetches a manifest or selects a matching manifest from an image index;
3. fetches the small configuration JSON;
4. verifies its SHA-256 against the manifest descriptor;
5. verifies OS, architecture, and optional variant against `--platform`;
6. converts layer descriptors from `oci-client` to `oci-spec`;
7. retains descriptors without downloading layer bytes.

Before assembly, `rib` also verifies that the base layer count equals the
number of `rootfs.diff_ids`. A mismatch indicates an invalid base image.

## `FROM scratch`

The exact word `scratch` means an entirely empty filesystem. `rib` creates no
registry client, downloads nothing, builds a fresh `ImageConfiguration` for
the selected platform, and starts with empty layers, diff IDs, and history. It
adds no base-image name or digest annotations.

`scratch:latest` and qualified references containing `scratch` are ordinary
registry image names. Once the empty base exists, local layers are added in the
same way as for any other image.

For a useful `scratch` image, supply a self-contained executable, an explicit
entrypoint, CA certificates for outbound TLS when needed, and every other
runtime file such as configuration, timezone, or user data. Rust applications
commonly use a musl target, but toolchain installation and cross-compilation
remain the user's responsibility.

## Updating the configuration

The assembler clones the base configuration or starts with an empty one, then:

1. sets `created`;
2. appends every new layer's `diff_id`;
3. appends one non-empty history entry per new layer when history exists;
4. applies requested runtime metadata;
5. verifies history/rootfs consistency;
6. serializes the configuration;
7. calculates its digest.

New history entries use:

```text
rib copy (<uncompressed-size> bytes uncompressed)
```

They omit host paths and the full CLI, avoiding publication of internal CI
paths or sensitive arguments.

## Merging runtime metadata

- An explicit `--entrypoint` replaces the base `Entrypoint`.
- Replacing it clears inherited `Cmd` unless `--keep-cmd` is set or a new
  `--cmd` is supplied. Explicit `--cmd` replaces the inherited value.
- `--label` merges into base labels and overwrites matching keys.
- `--port` extends base ports without duplicates.
- `--workdir` and `--user` change only when explicitly supplied.
- Other base metadata, including `Env`, is preserved.

For `scratch`, only explicitly supplied runtime fields exist, alongside the
builder-generated platform, creation time, rootfs, and history.

## History/rootfs invariant

Every history entry without `empty_layer: true` must correspond to exactly one
`rootfs.diff_ids` entry:

```text
count(history where empty_layer != true) == count(rootfs.diff_ids)
```

Metadata-only history does not count. A base configuration with no `history`
keeps it absent. A `scratch` configuration starts with an empty history array,
so every new layer receives an entry.

## Building the manifest

The assembler creates the configuration descriptor, keeps base layer
descriptors in order, appends new descriptors, adds the
`org.opencontainers.image.created` annotation, and records the real base name
and digest when applicable. It then serializes and hashes the manifest.

A `scratch` manifest has no base-image annotations. Each layer retains its
origin:

```text
LayerSource::Built(Layer)

LayerSource::FromBase {
    descriptor,
    source: ImageSource,
}
```

Output code can therefore read a local blob, lazily download a base blob, or
avoid a download through cross-repository mount without changing the manifest.
