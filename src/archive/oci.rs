use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use oci_spec::image::{
    ANNOTATION_REF_NAME, DescriptorBuilder, ImageIndexBuilder, MediaType, SCHEMA_VERSION,
};
use tracing::debug;

use super::output::write_image_tar;
use crate::blob::LayerProvider;
use crate::digest::blob_path;
use crate::image::assemble::ImageBundle;
use crate::tar_io::{EntryData, write_entry};

const OCI_LAYOUT_JSON: &[u8] = b"{\"imageLayoutVersion\":\"1.0.0\"}";

pub fn write_oci_archive(
    bundle: &ImageBundle,
    layers: &dyn LayerProvider,
    target: &Path,
    tag: Option<&str>,
) -> Result<()> {
    let index_json = build_index_json(bundle, tag)?;
    write_image_tar(target, "OCI archive", |archive| {
        write_entry(
            archive,
            "oci-layout",
            EntryData::Bytes(OCI_LAYOUT_JSON),
            None,
            None,
        )?;
        write_entry(
            archive,
            "index.json",
            EntryData::Bytes(&index_json),
            None,
            None,
        )?;
        write_entry(
            archive,
            &blob_path(&bundle.manifest_digest),
            EntryData::Bytes(&bundle.manifest_bytes),
            None,
            None,
        )?;
        write_entry(
            archive,
            &blob_path(&bundle.config_digest),
            EntryData::Bytes(&bundle.config_bytes),
            None,
            None,
        )?;

        let mut seen = HashSet::new();
        for layer in &bundle.layers {
            let descriptor = layer.descriptor();
            if !seen.insert(descriptor.digest().clone()) {
                debug!(digest = %descriptor.digest(), "blob already written, skipping");
                continue;
            }
            let mut file = layers
                .blob_file(layer)
                .with_context(|| format!("fetching layer {}", descriptor.digest()))?;
            write_entry(
                archive,
                &blob_path(descriptor.digest()),
                EntryData::File(&mut file),
                None,
                None,
            )?;
        }
        Ok(())
    })?;

    Ok(())
}

fn build_index_json(bundle: &ImageBundle, tag: Option<&str>) -> Result<Vec<u8>> {
    let mut descriptor = DescriptorBuilder::default()
        .media_type(MediaType::ImageManifest)
        .digest(bundle.manifest_digest.clone())
        .size(bundle.manifest_bytes.len() as u64);
    if let Some(tag) = tag {
        descriptor = descriptor.annotations(HashMap::from([(
            ANNOTATION_REF_NAME.to_string(),
            tag.to_string(),
        )]));
    }

    let descriptor = descriptor
        .build()
        .map_err(|error| anyhow::anyhow!("building index manifest descriptor: {error}"))?;
    let index = ImageIndexBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageIndex)
        .manifests(vec![descriptor])
        .build()
        .map_err(|error| anyhow::anyhow!("building image index: {error}"))?;
    serde_json::to_vec(&index).context("serializing index.json")
}
