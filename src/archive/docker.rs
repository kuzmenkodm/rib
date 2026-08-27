use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use oci_spec::image::Digest;
use serde::Serialize;
use tracing::debug;

use super::output::write_image_tar;
use crate::blob::LayerProvider;
use crate::image::assemble::ImageBundle;
use crate::tar_io::{EntryData, write_entry};

#[derive(Serialize)]
struct DockerManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags", skip_serializing_if = "Option::is_none")]
    repo_tags: Option<Vec<String>>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

pub fn write_docker_archive(
    bundle: &ImageBundle,
    layers: &dyn LayerProvider,
    target: &Path,
    tag: Option<&str>,
) -> Result<()> {
    let diff_ids: Vec<Digest> = bundle
        .config
        .rootfs()
        .diff_ids()
        .iter()
        .map(|value| {
            value
                .parse()
                .with_context(|| format!("parsing rootfs diff_id {value}"))
        })
        .collect::<Result<_>>()?;
    if diff_ids.len() != bundle.layers.len() {
        anyhow::bail!(
            "diff_id count {} != layer count {}; bundle is malformed",
            diff_ids.len(),
            bundle.layers.len()
        );
    }

    let config_filename = format!("{}.json", bundle.config_digest.digest());
    let layer_paths: Vec<String> = diff_ids
        .iter()
        .map(|digest| format!("{}/layer.tar", digest.digest()))
        .collect();
    let manifest_bytes = serde_json::to_vec(&[DockerManifestEntry {
        config: config_filename.clone(),
        repo_tags: tag.map(|value| vec![value.to_string()]),
        layers: layer_paths.clone(),
    }])
    .context("serializing docker manifest")?;

    write_image_tar(target, "Docker archive", |archive| {
        write_entry(
            archive,
            "manifest.json",
            EntryData::Bytes(&manifest_bytes),
            None,
            None,
        )?;
        write_entry(
            archive,
            &config_filename,
            EntryData::Bytes(&bundle.config_bytes),
            None,
            None,
        )?;

        let mut seen = HashSet::new();
        for ((layer, diff_id), path) in bundle
            .layers
            .iter()
            .zip(diff_ids.iter())
            .zip(layer_paths.iter())
        {
            if !seen.insert(diff_id.clone()) {
                debug!(%diff_id, "layer already written, skipping");
                continue;
            }
            let mut file = layers
                .uncompressed_file(layer)
                .with_context(|| format!("fetching uncompressed layer {diff_id}"))?;
            write_entry(archive, path, EntryData::File(&mut file), None, None)?;
        }
        Ok(())
    })?;

    Ok(())
}
