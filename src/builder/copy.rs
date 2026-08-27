use anyhow::{Context, Result, anyhow, bail};
use std::path::PathBuf;

pub use crate::tar_io::Owner;

#[derive(Debug, Clone)]
pub struct CopySpec {
    /// Source as written by the user. May be a glob
    pub src: String,
    /// Destination path inside the image
    pub dst: String,
    /// True iff the user wrote a trailing slash on dst
    pub dst_is_explicit_dir: bool,
    /// Optional file mode override (octal)
    pub mode: Option<u32>,
    /// Optional ownership
    pub chown: Option<Owner>,
}

#[derive(Debug)]
pub struct ResolvedEntry {
    pub src: PathBuf,
    /// Path inside the image for this entry. Always without leading slash,
    /// always normalized. For directories, this is the root under which the
    /// directory's contents are placed (Docker semantics: contents, not the
    /// directory itself)
    pub dst: String,
    /// True if `src` is a directory; we walk its contents
    pub is_dir: bool,
}

impl CopySpec {
    pub fn parse(s: &str) -> Result<Self> {
        let (src, rest) = s
            .split_once(':')
            .ok_or_else(|| anyhow!("invalid COPY directive, expected <src>:<dst>[,opts]: {s}"))?;
        if src.is_empty() {
            bail!("COPY source is empty: {s}");
        }

        // Destination paths containing commas are intentionally unsupported
        let (dst_raw, opts_raw) = match rest.split_once(',') {
            Some((d, o)) => (d, Some(o)),
            None => (rest, None),
        };
        if dst_raw.is_empty() {
            bail!("COPY destination is empty: {s}");
        }
        validate_image_destination(dst_raw)?;
        let dst_is_explicit_dir = dst_raw.ends_with('/');
        let dst = dst_raw.to_string();

        let mut mode: Option<u32> = None;
        let mut chown: Option<Owner> = None;
        if let Some(opts) = opts_raw {
            for kv in opts.split(',') {
                let kv = kv.trim();
                if kv.is_empty() {
                    continue;
                }
                let (k, v) = kv
                    .split_once('=')
                    .ok_or_else(|| anyhow!("invalid COPY option {kv:?}, expected key=value"))?;
                match k {
                    "mode" | "chmod" => {
                        let parsed = u32::from_str_radix(v, 8)
                            .with_context(|| format!("bad mode {v:?} (octal expected)"))?;
                        if parsed > 0o7777 {
                            bail!("bad mode {v:?} (must fit in 07777)");
                        }
                        mode = Some(parsed);
                    }
                    "chown" | "owner" => {
                        chown = Some(parse_owner(v)?);
                    }
                    other => bail!("unknown COPY option {other:?} (valid: mode, chown)"),
                }
            }
        }

        Ok(Self {
            src: src.to_string(),
            dst,
            dst_is_explicit_dir,
            mode,
            chown,
        })
    }

    pub(crate) fn executable(src: &std::path::Path, dst: &str) -> Result<Self> {
        validate_image_destination(dst)?;
        let src = src
            .to_str()
            .ok_or_else(|| anyhow!("artifact path is not valid UTF-8: {}", src.display()))?;
        Ok(Self {
            src: src.to_string(),
            dst: dst.to_string(),
            dst_is_explicit_dir: dst.ends_with('/'),
            mode: Some(0o755),
            chown: None,
        })
    }
}

fn validate_image_destination(dst: &str) -> Result<()> {
    if dst.contains('\0') {
        bail!("image destination contains a NUL byte");
    }
    let normalized = dst.replace('\\', "/");
    let relative = normalized.trim_matches('/');
    if relative.is_empty() {
        return Ok(());
    }
    for component in relative.split('/') {
        if component == "." || component == ".." {
            bail!("image destination must not contain {component:?}: {dst:?}");
        }
        if component.is_empty() {
            bail!("image destination contains an empty path component: {dst:?}");
        }
    }
    Ok(())
}

fn parse_owner(s: &str) -> Result<Owner> {
    let (uid_s, gid_s) = match s.split_once(':') {
        Some((u, g)) => (u, g),
        None => (s, "0"),
    };
    let uid = uid_s
        .parse::<u64>()
        .with_context(|| format!("bad chown uid {uid_s:?}"))?;
    let gid = gid_s
        .parse::<u64>()
        .with_context(|| format!("bad chown gid {gid_s:?}"))?;
    Ok(Owner { uid, gid })
}

pub fn has_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

pub fn strip_leading_slash(path: &str) -> &str {
    path.trim_start_matches('/')
}

fn strip_trailing_slash(path: &str) -> &str {
    path.trim_end_matches('/')
}

fn normalize_image_path(path: &str) -> String {
    path.trim_start_matches('/').replace('\\', "/")
}

pub fn join_posix(base: &str, suffix: &str) -> String {
    let base = base.trim_end_matches('/');
    let suffix = suffix.trim_start_matches('/').replace('\\', "/");
    match (base.is_empty(), suffix.is_empty()) {
        (true, _) => suffix,
        (_, true) => base.to_string(),
        (false, false) => format!("{base}/{suffix}"),
    }
}

/// Expand glob, then map each matched src path to its destination in the image
pub fn resolve_targets(spec: &CopySpec) -> Result<Vec<ResolvedEntry>> {
    let matches: Vec<PathBuf> = if has_glob_meta(&spec.src) {
        let mut v = Vec::new();
        for matched in
            glob::glob(&spec.src).with_context(|| format!("invalid glob pattern {:?}", spec.src))?
        {
            v.push(matched.with_context(|| format!("resolving glob pattern {:?}", spec.src))?);
        }
        v.sort(); // reproducible order
        if v.is_empty() {
            bail!("no files match {:?}", spec.src);
        }
        v
    } else {
        let p = PathBuf::from(&spec.src);
        if !p.exists() {
            bail!("source does not exist: {}", p.display());
        }
        vec![p]
    };

    let multiple = matches.len() > 1;
    let any_is_dir = matches.iter().any(|p| p.is_dir());
    let dst_is_dir = spec.dst_is_explicit_dir || multiple || any_is_dir;

    if multiple && !spec.dst_is_explicit_dir {
        // Docker: copying multiple sources requires dst to end with `/`
        // We're stricter than Docker (which would accept this if the dst
        // already exists as a dir on the host) — but we have no host fs to
        // consult, so we require the slash
        bail!(
            "multiple sources matched but dst {:?} is not a directory \
             (add a trailing slash)",
            spec.dst
        );
    }

    let mut out = Vec::with_capacity(matches.len());
    for src in matches {
        let file_type = std::fs::symlink_metadata(&src)
            .with_context(|| format!("stat {}", src.display()))?
            .file_type();
        if file_type.is_symlink() {
            bail!("symlink sources are not supported yet: {}", src.display());
        }
        if !file_type.is_file() && !file_type.is_dir() {
            bail!("special file sources are not supported: {}", src.display());
        }
        let is_dir = src.is_dir();
        let dst = if is_dir {
            normalize_image_path(strip_trailing_slash(&spec.dst))
        } else if dst_is_dir {
            let basename = src
                .file_name()
                .ok_or_else(|| anyhow!("source has no filename: {}", src.display()))?
                .to_string_lossy()
                .into_owned();
            normalize_image_path(&join_posix(strip_trailing_slash(&spec.dst), &basename))
        } else {
            normalize_image_path(&spec.dst)
        };
        out.push(ResolvedEntry { src, dst, is_dir });
    }
    Ok(out)
}
