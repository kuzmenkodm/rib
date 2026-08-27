# Building COPY layers

Each `--copy` option creates one deterministic gzip-compressed filesystem
layer. This chapter explains path resolution, metadata normalization, hashing,
and concurrency.

## Syntax

```text
<source>:<destination>[,mode=<octal>][,chown=<uid>[:<gid>]]
```

`source` may be a file, directory, or glob. `destination` is an absolute path
inside the image. `mode` overrides normalized permissions; `chown` sets numeric
UID and GID (GID defaults to UID when omitted).

```bash
--copy ./server:/usr/local/bin/server,mode=0755,chown=65532:65532
--copy ./config:/etc/server/
--copy 'target/release/*.so:/usr/local/lib/'
```

Quote globs so the shell does not expand them before `rib` receives them.
Absolute Windows source paths containing `:` are unsupported; use a relative
path from the appropriate directory.

## Resolving sources and destinations

A single file copied to a destination without a trailing slash receives that
exact path. A file copied to a destination ending in `/` keeps its basename.
Directory contents are rooted below the selected destination. Multiple glob
matches require a destination ending in `/`, preventing ambiguous replacement
of several files with one path.

Source paths from standalone CLI options are relative to the current working
directory. Cargo metadata paths are resolved relative to that package's
`Cargo.toml`; CLI additions to `cargo rib build` remain relative to the process
working directory.

Image paths are normalized and rejected if they contain empty, `.` or `..`
components. Symbolic links and special filesystem objects are rejected rather
than followed. Directory traversal is recursive, but only regular files and
directories enter the layer.

## Producing a deterministic tar stream

The complete entry list is collected and sorted by destination path before tar
writing. Tar metadata is normalized:

- modification time is `0`;
- UID/GID default to `0`, unless `chown` overrides them;
- user and group names are empty;
- explicit `mode` wins; otherwise directories use `0755`, executable files
  use `0755`, and other files use `0644`;
- entry order is stable.

Only the executable bit is inherited from a source file. Host ownership,
timestamps, umask, and traversal order do not leak into the image. Directory
entries are added as needed so extraction does not depend on implicit parent
creation.

## Hashing and compression in one pass

The uncompressed tar stream is hashed while being sent to gzip:

```text
source files
    │
    ▼
tar::Builder
    │
    ▼
CancellableWriter
    │
    ▼
SplitWriter
    ├──► HashingWriter ──► optional layer.tar
    │       └── diff_id + uncompressed_size
    │
    └──► GzEncoder
             ▼
          HashingWriter<File> ──► layer.tar.gz
             └── digest + compressed_size
```

`HashingWriter` forwards bytes while updating SHA-256 and a size counter.
Source data is read once. The uncompressed `layer.tar` is persisted only for a
Docker archive; otherwise `diff_id` is calculated without retaining that
stream. Gzip uses a fixed compression setting.

The result is represented as:

```text
Layer {
    diff_id,
    descriptor,
    uncompressed_size,
    storage: Arc<LayerStorage>,
}
```

## Concurrency preserves order

All copy directives enter a `JoinSet`, with a semaphore limiting active work to
`--jobs`. CPU- and disk-heavy work runs on the blocking thread pool. Each
result retains its input index:

```text
COPY #1 ───────────────► result #1 ─┐
COPY #2 ─────► result #2            ├──► [#1, #2, #3]
COPY #3 ─────────► result #3 ───────┘
```

The coordinator orders results by directive position, not completion time. If
two layers write the same image path, the later directive wins.

## Temporary disk usage

At minimum, every new layer needs a compressed temporary blob. Docker archive
output additionally needs the uncompressed tar:

```text
registry / OCI archive:
    sum of compressed COPY-layer sizes

Docker archive:
    compressed COPY layers + uncompressed COPY layers
```

Archive outputs also download base layers. Docker archives temporarily
decompress them one at a time. The final archive's temporary file is created
beside its destination and does not consume `TMPDIR`; layer temporary files do.
Plan CI storage for both filesystems. The `tempfile` crate follows the
`TMPDIR` environment variable on Unix.
