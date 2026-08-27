use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Seek, SeekFrom, copy};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use oci_spec::image::{Digest, MediaType};

use crate::cache::CacheController;
use crate::digest::short_digest;
use crate::image::Image;
use crate::image::layer::LayerSource;
use crate::progress::Progress;
use crate::registry::RegistryClient;

pub trait LayerProvider: Send + Sync {
    fn blob_file(&self, source: &LayerSource) -> Result<File>;
    fn uncompressed_file(&self, source: &LayerSource) -> Result<File>;
}

pub struct DownloadedLayers {
    files: HashMap<Digest, File>,
}

impl DownloadedLayers {
    pub fn empty() -> Self {
        Self {
            files: HashMap::new(),
        }
    }

    pub async fn fetch(
        client: &RegistryClient,
        base_image: &Image,
        jobs: usize,
        cache_controller: Option<CacheController>,
        progress: &Progress,
    ) -> Result<Self> {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(jobs));
        let mut tasks = tokio::task::JoinSet::new();
        let mut scheduled = HashSet::new();
        let mut layer_files = HashMap::new();
        let repository = base_image
            .source
            .as_ref()
            .context("base image layers are missing their registry source")?
            .repository_reference();

        for descriptor in &base_image.layers {
            let digest = descriptor.digest().clone();
            if !scheduled.insert(digest.clone()) {
                continue;
            }
            if let Some(cache) = &cache_controller
                && let Some(cached) = cache.cache_lookup(progress, digest.as_ref())
            {
                layer_files.insert(digest, File::open(cached)?);
                continue;
            }
            // With the cache enabled the blob is downloaded straight into the
            // cache directory; otherwise into an anonymous tempfile
            let file_path = cache_controller
                .as_ref()
                .map(|cache| cache.blob_path(digest.as_ref()));
            let task = progress.bytes(
                format!("pull layer {}", short_digest(&digest)),
                descriptor.size(),
            );
            let bar = task.progress_bar();
            let client = client.clone();
            let descriptor = descriptor.clone();
            let repository = repository.clone();
            let semaphore = semaphore.clone();
            tasks.spawn(async move {
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .context("blob download semaphore closed")?;
                let file = client
                    .fetch_layer_to_file(
                        &repository,
                        &descriptor,
                        file_path.as_ref(),
                        move |bytes| {
                            if let Some(bar) = &bar {
                                bar.inc(bytes);
                            }
                        },
                    )
                    .await?;

                task.done();
                Ok::<_, anyhow::Error>((digest, file))
            });
        }

        while let Some(result) = tasks.join_next().await {
            let (digest, file) = result.context("blob download task panicked")??;
            layer_files.insert(digest, file);
        }
        Ok(Self { files: layer_files })
    }
}

impl LayerProvider for DownloadedLayers {
    fn blob_file(&self, source: &LayerSource) -> Result<File> {
        match source {
            LayerSource::Built(layer) => layer.open_compressed(),
            LayerSource::FromBase { descriptor, .. } => Ok(self
                .files
                .get(descriptor.digest())
                .with_context(|| format!("layer {} was not downloaded", descriptor.digest()))?
                .try_clone()?),
        }
    }

    fn uncompressed_file(&self, source: &LayerSource) -> Result<File> {
        match source {
            LayerSource::Built(layer) => layer.open_uncompressed(),
            LayerSource::FromBase { .. } => {
                let mut blob = self.blob_file(source)?;
                decode_to_temp(&mut blob, source.descriptor().media_type())
            }
        }
    }
}

fn decode_to_temp(reader: &mut File, media_type: &MediaType) -> Result<File> {
    match media_type {
        MediaType::ImageLayer | MediaType::ImageLayerNonDistributable => {
            reader.seek(SeekFrom::Start(0))?;
            reader.try_clone().context("cloning uncompressed layer")
        }
        MediaType::ImageLayerGzip | MediaType::ImageLayerNonDistributableGzip => {
            gunzip_to_temp(reader)
        }
        MediaType::ImageLayerZstd | MediaType::ImageLayerNonDistributableZstd => {
            zstd_to_temp(reader)
        }
        MediaType::Other(value) if value.ends_with(".tar.gzip") || value.ends_with("+gzip") => {
            gunzip_to_temp(reader)
        }
        MediaType::Other(value) if value.ends_with("+zstd") => zstd_to_temp(reader),
        other => bail!("unsupported layer media type {other}"),
    }
}

fn gunzip_to_temp(reader: &mut File) -> Result<File> {
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| anyhow!("seek compressed data: {error}"))?;
    let mut decoder = flate2::read::GzDecoder::new(reader);
    copy_to_temp(&mut decoder).context("gunzip")
}

fn zstd_to_temp(reader: &mut File) -> Result<File> {
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| anyhow!("seek compressed data: {error}"))?;
    let mut decoder =
        zstd::stream::read::Decoder::new(reader).map_err(|error| anyhow!("zstd: {error}"))?;
    copy_to_temp(&mut decoder).context("zstd")
}

fn copy_to_temp(reader: &mut impl std::io::Read) -> Result<File> {
    let mut temporary = tempfile::tempfile().context("creating decompression tempfile")?;
    copy(reader, &mut temporary).context("decompressing layer")?;
    temporary
        .seek(SeekFrom::Start(0))
        .context("rewinding decompressed layer")?;
    Ok(temporary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn zstd_stream_is_decoded() {
        let mut compressed = tempfile::tempfile().unwrap();
        zstd::stream::copy_encode(b"uncompressed layer bytes".as_slice(), &mut compressed, 1)
            .unwrap();
        compressed.flush().unwrap();
        let mut decoded = zstd_to_temp(&mut compressed).unwrap();
        let mut bytes = Vec::new();
        decoded.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"uncompressed layer bytes");
    }
}
