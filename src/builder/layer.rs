use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use crate::builder::copy::{CopySpec, Owner, join_posix, resolve_targets, strip_leading_slash};
use crate::builder::writers::{CancelLableWriter, HashingWriter, SplitWriter};
use crate::image::layer::{Layer, LayerStorage};
use crate::tar_io::{EntryData, write_entry};
use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use oci_spec::image::{DescriptorBuilder, MediaType};
use tokio_util::sync::CancellationToken;
use walkdir::WalkDir;

#[cfg(test)]
pub fn build_copy_layer(spec: &CopySpec) -> Result<Layer> {
    build_copy_layer_with_options(spec, true, CancellationToken::new())
}

/// Build a deterministic layer while optionally retaining its uncompressed tar
pub(crate) fn build_copy_layer_with_options(
    spec: &CopySpec,
    retain_uncompressed: bool,
    cancellation: CancellationToken,
) -> Result<Layer> {
    let tmpdir = tempfile::tempdir().context("creating layer tempdir")?;
    let compressed_path = tmpdir.path().join("layer.tar.gz");
    let compressed_file = fs::File::create(&compressed_path)
        .with_context(|| format!("creating {}", compressed_path.display()))?;
    let uncompressed_path = retain_uncompressed.then(|| tmpdir.path().join("layer.tar"));
    let uncompressed_writer: Box<dyn Write> = match &uncompressed_path {
        Some(path) => Box::new(
            fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
        ),
        None => Box::new(std::io::sink()),
    };

    let compressed_hashing_writer = HashingWriter::new(compressed_file);
    let gz = GzEncoder::new(compressed_hashing_writer, Compression::fast());
    let uncompressed_hashing_writer = HashingWriter::new(uncompressed_writer);
    let split_writer = SplitWriter::new(uncompressed_hashing_writer, gz);
    let cancellable_writer = CancelLableWriter::new(split_writer, cancellation);
    let mut builder = tar::Builder::new(cancellable_writer);

    write_to_tar(&mut builder, spec)?;

    // Unwind. into_inner() calls finish() which writes tar trailer blocks
    let cancellable_writer = builder.into_inner().context("finalizing tar")?;
    let split_writer = cancellable_writer.into_inner();
    let (uncompressed_hashing_writer, gz) = split_writer.into_inner();
    let (diff_id, uncompressed_size) = uncompressed_hashing_writer.finalize();
    let compressed_hashing_writer = gz.finish().context("finalizing gzip")?;
    let (digest, size) = compressed_hashing_writer.finalize();

    Ok(Layer {
        diff_id,
        descriptor: DescriptorBuilder::default()
            .media_type(MediaType::ImageLayerGzip)
            .digest(digest)
            .size(size)
            .build()
            .context("building layer descriptor")?,
        uncompressed_size,
        storage: Arc::new(LayerStorage {
            _dir: tmpdir,
            compressed_path,
            uncompressed_path,
        }),
    })
}

fn write_dir_entries<W: Write>(
    builder: &mut tar::Builder<W>,
    src_root: &Path,
    dst_root: &str,
    spec: &CopySpec,
    seen: &mut HashSet<String>,
) -> Result<()> {
    // Sorted walk -> reproducible output regardless of filesystem order
    let walker = WalkDir::new(src_root).sort_by_file_name();
    for entry in walker {
        let entry = entry.with_context(|| format!("walking {}", src_root.display()))?;
        let rel = entry
            .path()
            .strip_prefix(src_root)
            .expect("walked path is under root");
        let dst_path = if rel.as_os_str().is_empty() {
            dst_root.to_string()
        } else {
            join_posix(dst_root, &rel.to_string_lossy())
        };
        let meta = entry.metadata()?;

        if entry.file_type().is_symlink() {
            anyhow::bail!(
                "symlink entries are not supported yet: {}",
                entry.path().display()
            );
        } else if meta.is_dir() {
            // Dedupe — both ensure_parent_dirs and our own walk could try to
            // write the same directory entry (e.g. dst_root itself)
            let key = strip_leading_slash(&dst_path)
                .trim_end_matches('/')
                .to_string();
            if !key.is_empty() && seen.insert(key) {
                write_entry(
                    builder,
                    &format!("{}/", dst_path.trim_end_matches('/')),
                    EntryData::Dir,
                    None,
                    spec.chown,
                )?;
            }
        } else if meta.is_file() {
            let mut file = std::fs::File::open(entry.path())
                .with_context(|| format!("open {}", entry.path().display()))?;
            write_entry(
                builder,
                &dst_path,
                EntryData::File(&mut file),
                spec.mode,
                spec.chown,
            )?;
        } else {
            anyhow::bail!(
                "special file entries are not supported: {}",
                entry.path().display()
            );
        }
    }
    Ok(())
}

/// Walk `dst` left-to-right and append a directory entry for each segment
/// except the last. So `/app/sub/file.bin` produces `app/` and `app/sub/`
/// Skips entries already in `seen` to avoid duplicates
fn ensure_parent_dirs<W: Write>(
    builder: &mut tar::Builder<W>,
    dst: &str,
    seen: &mut HashSet<String>,
    chown: Option<Owner>,
) -> Result<()> {
    let stripped = strip_leading_slash(dst);
    // Drop the final segment — it'll be written as the file/dir itself
    let parts: Vec<&str> = stripped
        .trim_end_matches('/')
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    if parts.len() <= 1 {
        return Ok(()); // file at root, no parents to make
    }
    let mut path = String::new();
    for part in &parts[..parts.len() - 1] {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(part);
        if seen.insert(path.clone()) {
            write_entry(builder, &format!("{path}/"), EntryData::Dir, None, chown)?;
        }
    }
    Ok(())
}

/// One concrete (src, dst-in-image) pair after globbing and dst-resolution
/// Created from a CopySpec by `resolve_targets`
fn write_to_tar<W: Write>(builder: &mut tar::Builder<W>, spec: &CopySpec) -> Result<()> {
    let entries = resolve_targets(spec)?;

    let mut dirs_written: HashSet<String> = HashSet::new();

    for entry in &entries {
        if entry.is_dir {
            ensure_parent_dirs(builder, &entry.dst, &mut dirs_written, spec.chown)?;
            // Directory itself is the dst — write it as a directory entry,
            // then walk its CONTENTS into it
            write_dir_entries(builder, &entry.src, &entry.dst, spec, &mut dirs_written)?;
        } else {
            ensure_parent_dirs(builder, &entry.dst, &mut dirs_written, spec.chown)?;
            let mut file = std::fs::File::open(&entry.src)
                .with_context(|| format!("open {}", entry.src.display()))?;
            write_entry(
                builder,
                &entry.dst,
                EntryData::File(&mut file),
                spec.mode,
                spec.chown,
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_tmp(dir: &TempDir, name: &str, content: &[u8]) -> std::path::PathBuf {
        let p = dir.path().join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn parse_basic() {
        let s = CopySpec::parse("./app:/usr/bin/app").unwrap();
        assert_eq!(s.src, "./app");
        assert_eq!(s.dst, "/usr/bin/app");
        assert!(!s.dst_is_explicit_dir);
        assert_eq!(s.mode, None);
        assert_eq!(s.chown, None);
    }

    #[test]
    fn parse_trailing_slash_marks_dst_as_dir() {
        let s = CopySpec::parse("./app:/usr/bin/").unwrap();
        assert!(s.dst_is_explicit_dir);
        assert_eq!(s.dst, "/usr/bin/");
    }

    #[test]
    fn parse_mode_via_options() {
        let s = CopySpec::parse("./app:/usr/bin/app,mode=0755").unwrap();
        assert_eq!(s.mode, Some(0o755));
    }

    #[test]
    fn parse_chmod_alias_works() {
        // Dockerfile users know `--chmod=`; accept it as alias for `mode`
        let s = CopySpec::parse("./app:/x,chmod=0700").unwrap();
        assert_eq!(s.mode, Some(0o700));
    }

    #[test]
    fn parse_chown_uid_gid() {
        let s = CopySpec::parse("./app:/x,chown=1000:2000").unwrap();
        assert_eq!(
            s.chown,
            Some(Owner {
                uid: 1000,
                gid: 2000
            })
        );
    }

    #[test]
    fn parse_chown_uid_only_defaults_gid_to_zero() {
        let s = CopySpec::parse("./app:/x,chown=1000").unwrap();
        assert_eq!(s.chown, Some(Owner { uid: 1000, gid: 0 }));
    }

    #[test]
    fn parse_combined_mode_and_chown() {
        let s = CopySpec::parse("./app:/x,mode=0755,chown=1000:1000").unwrap();
        assert_eq!(s.mode, Some(0o755));
        assert_eq!(
            s.chown,
            Some(Owner {
                uid: 1000,
                gid: 1000
            })
        );
    }

    #[test]
    fn parse_rejects_bad_input() {
        assert!(CopySpec::parse("just-a-path").is_err());
        assert!(CopySpec::parse(":/dst").is_err());
        assert!(CopySpec::parse("./src:").is_err());
        assert!(CopySpec::parse("./src:/dst,mode=99x").is_err()); // non-octal
        assert!(CopySpec::parse("./src:/dst,mode=017777").is_err());
        assert!(CopySpec::parse("./src:/dst,chown=alice").is_err()); // non-numeric
        assert!(CopySpec::parse("./src:/dst,unknownopt=1").is_err());
        assert!(CopySpec::parse("./src:/app/../escape").is_err());
        assert!(CopySpec::parse("./src:/app//file").is_err());
    }

    fn make_tree() -> TempDir {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("b.txt"), b"b").unwrap();
        std::fs::write(dir.path().join("ignore.md"), b"i").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/c.txt"), b"c").unwrap();
        dir
    }

    fn paths_in_layer(layer: &Layer) -> Vec<String> {
        let mut f = layer.open_uncompressed().unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut bytes).unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn dst_with_trailing_slash_keeps_basename() {
        // file -> /app/  →  /app/<basename>
        let dir = make_tree();
        let spec = CopySpec::parse(&format!("{}/a.txt:/app/", dir.path().display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();
        let paths = paths_in_layer(&layer);
        assert!(paths.contains(&"app/a.txt".to_string()), "got {paths:?}");
        // No "app/" without basename means dst was treated as a directory
        assert!(!paths.contains(&"app".to_string()));
    }

    #[test]
    fn dst_without_trailing_slash_is_exact_filename() {
        // file -> /app/foo.txt  →  /app/foo.txt (renamed)
        let dir = make_tree();
        let spec =
            CopySpec::parse(&format!("{}/a.txt:/app/foo.txt", dir.path().display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();
        let paths = paths_in_layer(&layer);
        assert!(paths.contains(&"app/foo.txt".to_string()), "got {paths:?}");
    }

    #[test]
    fn directory_source_copies_contents_not_itself() {
        // Docker rule: COPY mydir/ /app/ puts contents of mydir into /app/,
        // it does NOT create /app/mydir/
        let dir = make_tree();
        let spec = CopySpec::parse(&format!("{}:/app/", dir.path().display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();
        let paths = paths_in_layer(&layer);

        // Files end up directly in /app/, not in /app/<basename of dir>/
        assert!(paths.contains(&"app/a.txt".to_string()), "got {paths:?}");
        assert!(paths.contains(&"app/b.txt".to_string()));
        assert!(paths.contains(&"app/sub/c.txt".to_string()));
        // No nesting under the source dir's name
        let dir_basename = dir
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            !paths
                .iter()
                .any(|p| p.starts_with(&format!("app/{dir_basename}"))),
            "must not nest under source dir name: {paths:?}"
        );
    }

    #[test]
    fn directory_source_copies_contents_to_image_root() {
        let dir = make_tree();
        let spec = CopySpec::parse(&format!("{}:/", dir.path().display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();
        let paths = paths_in_layer(&layer);

        assert!(paths.contains(&"a.txt".to_string()), "got {paths:?}");
        assert!(paths.contains(&"b.txt".to_string()));
        assert!(paths.contains(&"sub/c.txt".to_string()));
        assert!(
            paths.iter().all(|path| !path.starts_with('/')),
            "tar paths must be relative: {paths:?}"
        );
    }

    fn assert_long_destination(destination: &str) {
        let dir = TempDir::new().unwrap();
        let source = write_tmp(&dir, "payload", b"payload");
        let spec = CopySpec::parse(&format!("{}:{destination}", source.display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();
        let paths = paths_in_layer(&layer);
        let expected = destination.trim_start_matches('/');

        assert!(
            paths.iter().any(|path| path == expected),
            "missing {expected:?} in {paths:?}"
        );
        assert!(paths.iter().all(|path| !path.starts_with('/')));
    }

    #[test]
    fn supports_gnu_tar_paths_longer_than_100_bytes() {
        let destination = format!("/{}/payload", "a".repeat(101));
        assert!(destination.len() > 100);
        assert_long_destination(&destination);
    }

    #[test]
    fn supports_gnu_tar_paths_longer_than_255_bytes() {
        let component = "segment".repeat(10);
        let destination = format!("/{component}/{component}/{component}/{component}/payload");
        assert!(destination.len() > 255);
        assert_long_destination(&destination);
    }

    #[test]
    fn glob_expands_to_multiple_files() {
        let dir = make_tree();
        let spec = CopySpec::parse(&format!("{}/*.txt:/app/", dir.path().display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();
        let paths = paths_in_layer(&layer);
        // Both .txt files matched; .md was skipped
        assert!(paths.contains(&"app/a.txt".to_string()), "got {paths:?}");
        assert!(paths.contains(&"app/b.txt".to_string()));
        assert!(!paths.iter().any(|p| p.contains("ignore.md")));
    }

    #[test]
    fn glob_with_no_matches_errors() {
        let dir = TempDir::new().unwrap();
        let spec = CopySpec::parse(&format!("{}/*.nope:/app/", dir.path().display())).unwrap();
        assert!(build_copy_layer(&spec).is_err());
    }

    #[test]
    fn glob_with_multiple_matches_requires_dir_dst() {
        // `*.txt → /app/foo` (no slash) is ambiguous — Docker rejects this,
        // and so do we. Caller must add a trailing slash to dst
        let dir = make_tree();
        let spec = CopySpec::parse(&format!("{}/*.txt:/app/foo", dir.path().display())).unwrap();
        let err = build_copy_layer(&spec).unwrap_err();
        assert!(
            format!("{err}").contains("not a directory"),
            "expected 'not a directory' in error, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_source_fails_instead_of_being_silently_skipped() {
        use std::os::unix::fs::symlink;

        let dir = make_tree();
        let link = dir.path().join("link.txt");
        symlink(dir.path().join("a.txt"), &link).unwrap();
        let spec = CopySpec::parse(&format!("{}:/app/link.txt", link.display())).unwrap();
        let error = build_copy_layer(&spec).unwrap_err();
        assert!(error.to_string().contains("symlink"));
    }

    #[test]
    fn chown_propagates_to_file_and_parent_dirs() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("x"), b"x").unwrap();
        let spec = CopySpec::parse(&format!(
            "{}/x:/app/bin/x,chown=1000:2000",
            dir.path().display()
        ))
        .unwrap();
        let layer = build_copy_layer(&spec).unwrap();

        let mut f = layer.open_uncompressed().unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut f, &mut bytes).unwrap();
        let mut archive = tar::Archive::new(bytes.as_slice());
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            assert_eq!(entry.header().uid().unwrap(), 1000);
            assert_eq!(entry.header().gid().unwrap(), 2000);
        }
    }

    #[test]
    fn builds_from_single_file() {
        let dir = TempDir::new().unwrap();
        let src = write_tmp(&dir, "hello.txt", b"hello");
        let spec = CopySpec::parse(&format!("{}:/app/hello.txt", src.display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();

        assert!(layer.diff_id.to_string().starts_with("sha256:"));
        assert!(layer.descriptor.digest().to_string().starts_with("sha256:"));
        assert!(layer.size() > 0);
        assert!(layer.uncompressed_size > 0);
    }

    /// The canonical OCI bug: producers that put the gzipped digest into
    /// rootfs.diff_ids. Should never happen here
    #[test]
    fn diff_id_differs_from_digest() {
        let dir = TempDir::new().unwrap();
        let src = write_tmp(&dir, "x.txt", b"some content here");
        let spec = CopySpec::parse(&format!("{}:/x.txt", src.display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();
        assert_ne!(
            layer.diff_id.to_string(),
            layer.descriptor.digest().to_string(),
            "diff_id (uncompressed) must never equal digest (gzipped)"
        );
    }

    #[test]
    fn skipping_uncompressed_storage_preserves_layer_identity() {
        let dir = TempDir::new().unwrap();
        let source = write_tmp(&dir, "payload", b"payload");
        let spec = CopySpec::parse(&format!("{}:/payload", source.display())).unwrap();

        let retained =
            build_copy_layer_with_options(&spec, true, CancellationToken::new()).unwrap();
        let discarded =
            build_copy_layer_with_options(&spec, false, CancellationToken::new()).unwrap();

        assert!(retained.storage.uncompressed_path.is_some());
        assert!(discarded.storage.uncompressed_path.is_none());
        assert!(discarded.storage.compressed_path.is_file());
        assert!(discarded.open_uncompressed().is_err());
        assert_eq!(discarded.diff_id, retained.diff_id);
        assert_eq!(discarded.descriptor, retained.descriptor);
        assert_eq!(discarded.uncompressed_size, retained.uncompressed_size);
    }

    #[test]
    fn cancelled_layer_build_stops_before_writing_data() {
        let dir = TempDir::new().unwrap();
        let source = write_tmp(&dir, "payload", b"payload");
        let spec = CopySpec::parse(&format!("{}:/payload", source.display())).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = build_copy_layer_with_options(&spec, false, cancellation).unwrap_err();

        assert!(format!("{error:#}").contains("layer build cancelled"));
    }

    #[test]
    fn explicit_mode_is_applied() {
        let dir = TempDir::new().unwrap();
        let src = write_tmp(&dir, "x", b"x");
        let spec = CopySpec::parse(&format!("{}:/x,mode=0700", src.display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();

        let uncompressed = layer.uncompressed_bytes().unwrap();
        let mut archive = tar::Archive::new(uncompressed.as_slice());
        let entry = archive.entries().unwrap().next().unwrap().unwrap();
        let mode = entry.header().mode().unwrap();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn entries_have_zero_mtime_uid_gid() {
        let dir = TempDir::new().unwrap();
        let src = write_tmp(&dir, "x", b"x");
        let spec = CopySpec::parse(&format!("{}:/x", src.display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();

        let uncompressed = layer.uncompressed_bytes().unwrap();
        let mut archive = tar::Archive::new(uncompressed.as_slice());
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            let h = entry.header();
            assert_eq!(h.mtime().unwrap(), 0);
            assert_eq!(h.uid().unwrap(), 0);
            assert_eq!(h.gid().unwrap(), 0);
        }
    }

    #[test]
    fn directory_walk_is_sorted() {
        let dir = TempDir::new().unwrap();
        // Create out of order to make sure we're not just observing FS order
        std::fs::write(dir.path().join("z.txt"), b"z").unwrap();
        std::fs::write(dir.path().join("a.txt"), b"a").unwrap();
        std::fs::write(dir.path().join("m.txt"), b"m").unwrap();

        let spec = CopySpec::parse(&format!("{}:/x", dir.path().display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();

        let uncompressed = layer.uncompressed_bytes().unwrap();
        let mut archive = tar::Archive::new(uncompressed.as_slice());
        let order: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.header().entry_type() == tar::EntryType::Regular)
            .map(|e| {
                e.path()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(order, vec!["a.txt", "m.txt", "z.txt"]);
    }

    #[test]
    fn destination_paths_are_stripped_of_leading_slash() {
        // OCI/Docker tar convention: no leading slash on entry paths
        let dir = TempDir::new().unwrap();
        let src = write_tmp(&dir, "x", b"x");
        let spec = CopySpec::parse(&format!("{}:/usr/local/bin/app", src.display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();

        let uncompressed = layer.uncompressed_bytes().unwrap();
        let mut archive = tar::Archive::new(uncompressed.as_slice());
        let entries: Vec<(String, tar::EntryType)> = archive
            .entries()
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                let path = e.path().unwrap().to_string_lossy().into_owned();
                (path, e.header().entry_type())
            })
            .collect();
        // Parent directories are materialized in order, then the file itself
        let paths: Vec<&String> = entries.iter().map(|(p, _)| p).collect();
        assert_eq!(
            paths,
            vec!["usr/", "usr/local/", "usr/local/bin/", "usr/local/bin/app"],
            "got {entries:?}"
        );
        // Last entry must be the regular file with the right path
        let last = entries.last().unwrap();
        assert_eq!(last.0, "usr/local/bin/app");
        assert_eq!(last.1, tar::EntryType::Regular);
    }

    /// Parent dirs created when --copy targets nested paths must have sane
    /// permissions (0o755), not the broken "----------" some viewers fall
    /// back to when no directory entry exists at all
    #[test]
    fn parent_directories_have_proper_permissions() {
        let dir = TempDir::new().unwrap();
        let src = write_tmp(&dir, "x", b"x");
        let spec = CopySpec::parse(&format!("{}:/app/bin/server", src.display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();

        let uncompressed = layer.uncompressed_bytes().unwrap();
        let mut archive = tar::Archive::new(uncompressed.as_slice());
        let dir_entries: Vec<(String, u32)> = archive
            .entries()
            .unwrap()
            .map(|e| e.unwrap())
            .filter(|e| e.header().entry_type() == tar::EntryType::Directory)
            .map(|e| {
                let p = e.path().unwrap().to_string_lossy().into_owned();
                let m = e.header().mode().unwrap();
                (p, m & 0o777)
            })
            .collect();
        assert_eq!(
            dir_entries.len(),
            2,
            "expected app/ and app/bin/, got {dir_entries:?}"
        );
        for (path, mode) in &dir_entries {
            assert_eq!(*mode, 0o755, "{path} has mode {mode:o}, expected 755");
        }
    }

    /// Ignored by default. Run with: `cargo test smoke_dump_to_tmp -- --ignored --nocapture`
    /// Builds a small layer and writes the gzipped tar to /tmp for inspection
    /// with stock tools (`tar -tzvf`, `gzip -lv`, etc)
    #[test]
    #[ignore]
    fn smoke_dump_to_tmp() {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("greeting.txt"), b"hello\n").unwrap();
        std::fs::write(dir.path().join("sub/nested.txt"), b"nested\n").unwrap();

        let spec = CopySpec::parse(&format!("{}:/app", dir.path().display())).unwrap();
        let layer = build_copy_layer(&spec).unwrap();

        let compressed = layer.compressed_bytes().unwrap();
        std::fs::write("/tmp/rib-smoke.tar.gz", &compressed).unwrap();
        eprintln!("diff_id      = {}", layer.diff_id);
        eprintln!("digest       = {}", layer.descriptor.digest());
        eprintln!("uncompressed = {} bytes", layer.uncompressed_size);
        eprintln!("compressed   = {} bytes", layer.size());
        eprintln!("wrote /tmp/rib-smoke.tar.gz");
    }
}
