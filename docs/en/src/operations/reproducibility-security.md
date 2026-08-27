# Reproducibility and security

## What a reproducible result means

With identical inputs and the same `rib` and dependency versions,
reproducibility means byte-for-byte identical layers, configuration, manifest,
and digest—not merely equivalent filesystem contents.

`rib` removes common tar/gzip variability:

- tar entries are sorted;
- every entry has `mtime = 0`;
- default UID/GID is `0`, and user/group names are empty;
- permissions follow fixed normalization rules;
- gzip uses a fixed compression setting;
- image creation time defaults to the Unix epoch;
- concurrently built layers are restored to input order.

`--creation-time now` intentionally makes configuration and manifest depend on
wall-clock time. For reproducibility, use `epoch` or a fixed RFC 3339 value such
as the commit time.

## Factors that affect output

Result bytes depend on source contents and executable bits, copy list and
order, explicit mode/ownership, platform, base configuration and descriptors,
runtime metadata, and the exact `rib` and serialization/compression dependency
versions.

Base tags are mutable. Pin a digest for strict repeatability:

```bash
rib build \
  --from alpine@sha256:<manifest-digest> \
  --platform linux/amd64 \
  ...
```

Treat the builder version as part of provenance and pin it in CI.

## Data integrity

Downloaded base configuration bytes are rehashed and checked against the
manifest descriptor. Assembly verifies layer/diff-ID counts and history
consistency. The summary goes to stderr; redirected stdout receives only the
manifest digest, which can be stored as an immutable deployment reference.

`rib` does not sign or create provenance. Pass the digest to a dedicated tool:

```bash
IMAGE_DIGEST="$(rib build ... --to registry:registry.example.com/team/app:v1)"
cosign sign "registry.example.com/team/app@${IMAGE_DIGEST}"
```

## Credentials

- Prefer a read-only Docker configuration or an external secret store over a
  literal password.
- Never store credentials in `Cargo.toml`; metadata deliberately excludes them.
- Do not use `set -x` around `--credential`.
- Scope tokens to the required repositories and pull/push permissions.
- Where the threat model requires it, use separate source and target registry
  accounts. Credentials are selected by registry host, so separate identities
  for two repositories on the same host are not supported within one run.

## TLS

HTTPS with certificate verification is the default. Plain HTTP removes
encryption; `--*-skip-tls` keeps HTTPS but disables server identity validation.
Source and destination roles are configured separately, but an insecure target
option affects every target registry in that invocation. Do not combine a
development target requiring `--to-skip-tls` with a production target in the
same command.

## `--copy` as a trust boundary

`rib` reads every explicitly selected file and every glob match. There is no
`.dockerignore` equivalent. Broad source directories can accidentally include
`.env`, private keys, credentials, source code, `.git`, or logs. Prefer narrow
paths and inspect resulting images or archives in CI.

Symbolic-link sources and special files are rejected. Destination paths with
empty, `.` or `..` components are also rejected, preventing unexpected
filesystem traversal.

## `scratch` and runtime security

`scratch` minimizes included files but does not automatically secure an
application. Keep static dependencies updated, add CA certificates when
needed, set a non-root user, exclude secrets, provide application-level logs,
metrics and health checks, and scan the executable itself.

Minimal images contain no shell for interactive diagnosis. In Kubernetes,
plan for ephemeral debug containers rather than relying on `kubectl exec` into
the application image.

## Publication safety

An archive replaces its target only after a complete write, file
synchronization, and atomic rename. A registry manifest is published only after
all layers and configuration are available. These mechanisms prevent partial
results from becoming visible, but multiple `--to` destinations are still not
one transaction and cannot roll one another back.
