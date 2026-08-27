use anyhow::{Context, Result};
use std::{collections::HashSet, path::PathBuf};

use crate::{digest::sha256_digest_file, progress::Progress};

pub const DEFAULT_CACHE_DIR: &str = ".rib-cache";

/// Digests contain a colon (`sha256:…`), which is not a legal filename
/// character on Windows, so blobs are stored under `sha256-…` instead
fn blob_file_name(layer_digest: &str) -> String {
    layer_digest.replace(':', "-")
}

#[derive(Clone)]
pub struct CacheController {
    cache_dir: PathBuf,
    cached_layers: HashSet<String>,
}

impl CacheController {
    pub fn new(cache_dir_path: Option<&str>) -> Result<Self> {
        let mut controller = CacheController {
            cache_dir: PathBuf::from(cache_dir_path.unwrap_or(DEFAULT_CACHE_DIR)),
            cached_layers: HashSet::new(),
        };
        controller.create_cache_dir()?;
        controller.index_cache_dir()?;
        Ok(controller)
    }

    /// Path where the blob with this digest is (or should be) stored
    pub fn blob_path(&self, layer_digest: &str) -> PathBuf {
        self.cache_dir.join(blob_file_name(layer_digest))
    }

    fn create_cache_dir(&self) -> Result<()> {
        std::fs::create_dir_all(&self.cache_dir)
            .with_context(|| format!("creating cache dir {:?}", self.cache_dir))?;
        Ok(())
    }

    fn index_cache_dir(&mut self) -> Result<()> {
        for entry in std::fs::read_dir(&self.cache_dir)
            .with_context(|| format!("reading cache dir {:?}", self.cache_dir))?
        {
            let entry = entry?;
            if entry.file_type()?.is_file() {
                self.cached_layers
                    .insert(entry.file_name().to_string_lossy().into_owned());
            }
        }

        Ok(())
    }

    pub fn cache_lookup(&self, progress: &Progress, layer_digest: &str) -> Option<PathBuf> {
        if !self.cached_layers.contains(&blob_file_name(layer_digest)) {
            return None;
        }
        let file_path = self.blob_path(layer_digest);
        match sha256_digest_file(&file_path) {
            Ok(cached_layer_sha) if cached_layer_sha.as_ref() == layer_digest => {
                progress.note(format!("cache hit for layer `{layer_digest}`"));
                Some(file_path)
            }
            Ok(cached_layer_sha) => {
                progress.err_note(format!(
                    "cached layer `{layer_digest}` hashes to `{cached_layer_sha}`; re-downloading"
                ));
                None
            }
            Err(error) => {
                progress.err_note(format!(
                    "hashing cached layer {file_path:?} failed ({error}); re-downloading"
                ));
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::sha256_digest;
    use crate::progress::{Progress, ProgressMode};
    use tempfile::TempDir;

    #[test]
    fn blob_paths_avoid_colons_in_file_names() {
        let directory = TempDir::new().unwrap();
        let controller = CacheController::new(directory.path().to_str()).unwrap();
        let path = controller.blob_path("sha256:abcdef");
        assert_eq!(path.file_name().unwrap().to_string_lossy(), "sha256-abcdef");
    }

    #[test]
    fn lookup_returns_only_entries_whose_content_matches_the_digest() {
        let directory = TempDir::new().unwrap();
        let cache_dir = directory.path().to_str();
        let payload = b"layer bytes";
        let digest = sha256_digest(payload);
        let progress = Progress::new(ProgressMode::Quiet);

        let controller = CacheController::new(cache_dir).unwrap();
        std::fs::write(controller.blob_path(digest.as_ref()), payload).unwrap();

        // A fresh controller indexes the directory and serves the hit
        let controller = CacheController::new(cache_dir).unwrap();
        assert!(
            controller
                .cache_lookup(&progress, digest.as_ref())
                .is_some()
        );

        // A corrupted entry is rejected instead of being served
        std::fs::write(controller.blob_path(digest.as_ref()), b"garbage").unwrap();
        assert!(
            controller
                .cache_lookup(&progress, digest.as_ref())
                .is_none()
        );

        // An unknown digest is a miss
        let unknown = sha256_digest(b"never cached");
        assert!(
            controller
                .cache_lookup(&progress, unknown.as_ref())
                .is_none()
        );
    }
}
