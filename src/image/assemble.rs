use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use oci_spec::image::{
    ANNOTATION_BASE_IMAGE_DIGEST, ANNOTATION_BASE_IMAGE_NAME, ANNOTATION_CREATED, Descriptor,
    DescriptorBuilder, Digest, History, HistoryBuilder, ImageConfiguration, ImageManifestBuilder,
    MediaType, SCHEMA_VERSION,
};

use crate::digest::sha256_digest;
use crate::image::Image;
use crate::image::layer::{Layer, LayerSource};

#[derive(Debug)]
pub struct AssembleOptions {
    pub base: Image,
    pub new_layers: Vec<Layer>,
    pub entrypoint: Option<Vec<String>>,
    pub cmd: Option<Vec<String>>,
    pub labels: BTreeMap<String, String>,
    pub ports: Vec<String>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub creation_time: Option<String>,
    pub keep_cmd: bool,
}

#[derive(Debug, Clone)]
pub struct ImageBundle {
    pub config: ImageConfiguration,
    pub config_bytes: Bytes,
    pub config_digest: Digest,
    pub manifest_bytes: Bytes,
    pub manifest_digest: Digest,
    pub layers: Vec<LayerSource>,
}

pub fn assemble(options: AssembleOptions) -> Result<ImageBundle> {
    validate_layer_count(&options.base.config, options.base.layers.len())?;

    let mut config = options.base.config.clone();
    let created = options
        .creation_time
        .clone()
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());
    config.set_created(Some(created.clone()));

    let mut diff_ids = config.rootfs().diff_ids().clone();
    diff_ids.extend(
        options
            .new_layers
            .iter()
            .map(|layer| layer.diff_id.to_string()),
    );
    config.rootfs_mut().set_diff_ids(diff_ids);

    if let Some(mut history) = config.history().clone() {
        for layer in &options.new_layers {
            history.push(history_entry(&created, layer)?);
        }
        config.set_history(Some(history));
    }

    let mut runtime = config.config().clone().unwrap_or_default();
    if let Some(entrypoint) = &options.entrypoint {
        runtime.set_entrypoint(Some(entrypoint.clone()));
        // Docker semantics: overriding the entrypoint invalidates the Cmd
        // inherited from the base image, unless the caller keeps it
        if !options.keep_cmd && options.cmd.is_none() {
            runtime.set_cmd(None);
        }
    }
    if options.cmd.is_some() {
        runtime.set_cmd(options.cmd);
    }
    if !options.labels.is_empty() {
        let mut labels: HashMap<String, String> = runtime.labels().clone().unwrap_or_default();
        labels.extend(
            options
                .labels
                .iter()
                .map(|(key, value)| (key.clone(), value.clone())),
        );
        runtime.set_labels(Some(labels));
    }

    if !options.ports.is_empty() {
        let mut ports = runtime.exposed_ports().clone().unwrap_or_default();
        for port in &options.ports {
            if !ports.contains(port) {
                ports.push(port.clone());
            }
        }
        runtime.set_exposed_ports(Some(ports));
    }

    if let Some(workdir) = &options.workdir {
        runtime.set_working_dir(Some(workdir.clone()));
    }
    if let Some(user) = &options.user {
        runtime.set_user(Some(user.clone()));
    }
    config.set_config(Some(runtime));

    validate_history(&config)?;

    let config_bytes = canonical_json(&config).context("serializing image config")?;
    let config_digest = sha256_digest(&config_bytes);
    let config_descriptor = DescriptorBuilder::default()
        .media_type(MediaType::ImageConfig)
        .digest(config_digest.clone())
        .size(config_bytes.len() as u64)
        .build()
        .map_err(|error| anyhow!("building config descriptor: {error}"))?;

    let mut layers = Vec::with_capacity(options.base.layers.len() + options.new_layers.len());
    if !options.base.layers.is_empty() {
        let source = options
            .base
            .source
            .as_ref()
            .context("base image layers are missing their registry source")?;
        layers.extend(
            options
                .base
                .layers
                .iter()
                .map(|descriptor| LayerSource::FromBase {
                    descriptor: Box::new(descriptor.clone()),
                    source: source.clone(),
                }),
        );
    }
    layers.extend(
        options
            .new_layers
            .into_iter()
            .map(|layer| LayerSource::Built(Box::new(layer))),
    );
    let layer_descriptors: Vec<Descriptor> = layers
        .iter()
        .map(|layer| layer.descriptor().clone())
        .collect();

    let mut annotations = HashMap::from([(ANNOTATION_CREATED.to_string(), created)]);
    if let Some(source) = &options.base.source {
        annotations.insert(
            ANNOTATION_BASE_IMAGE_DIGEST.to_string(),
            source.manifest_digest.to_string(),
        );
        annotations.insert(
            ANNOTATION_BASE_IMAGE_NAME.to_string(),
            source.reference.to_string(),
        );
    }
    let manifest = ImageManifestBuilder::default()
        .schema_version(SCHEMA_VERSION)
        .media_type(MediaType::ImageManifest)
        .config(config_descriptor)
        .layers(layer_descriptors)
        .annotations(annotations)
        .build()
        .map_err(|error| anyhow!("building manifest: {error}"))?;
    let manifest_bytes = canonical_json(&manifest).context("serializing manifest")?;
    let manifest_digest = sha256_digest(&manifest_bytes);

    Ok(ImageBundle {
        config,
        config_bytes: Bytes::from(config_bytes),
        config_digest,
        manifest_bytes: Bytes::from(manifest_bytes),
        manifest_digest,
        layers,
    })
}

fn validate_layer_count(config: &ImageConfiguration, layer_count: usize) -> Result<()> {
    let diff_ids = config.rootfs().diff_ids().len();
    if diff_ids != layer_count {
        bail!(
            "base config diff_ids count does not match manifest layers ({diff_ids} != {layer_count})"
        );
    }
    Ok(())
}

fn validate_history(config: &ImageConfiguration) -> Result<()> {
    let Some(history) = config.history() else {
        return Ok(());
    };
    let non_empty = history
        .iter()
        .filter(|entry| entry.empty_layer() != Some(true))
        .count();
    let diff_ids = config.rootfs().diff_ids().len();
    if non_empty != diff_ids {
        bail!(
            "non-empty history count does not match diff_ids ({non_empty} != {diff_ids}); base config likely malformed"
        );
    }
    Ok(())
}

fn history_entry(created: &str, layer: &Layer) -> Result<History> {
    HistoryBuilder::default()
        .created(created.to_string())
        .created_by(format!(
            "rib copy ({} bytes uncompressed)",
            layer.uncompressed_size
        ))
        .build()
        .map_err(|error| anyhow!("building history entry: {error}"))
}

fn canonical_json<T: serde::Serialize>(value: &T) -> Result<Vec<u8>> {
    let value = serde_json::to_value(value)?;
    Ok(serde_json::to_vec(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::copy::CopySpec;
    use crate::builder::layer::build_copy_layer;
    use crate::digest::sha256_digest;
    use crate::image::{ImageSource, Platform};
    use oci_spec::image::{Arch, ConfigBuilder, ImageConfigurationBuilder, Os, RootFsBuilder};
    use tempfile::TempDir;

    fn base(layer_count: usize, with_history: bool) -> Image {
        let diff_ids: Vec<String> = (0..layer_count)
            .map(|index| sha256_digest(format!("diff-{index}").as_bytes()).to_string())
            .collect();
        let layers: Vec<Descriptor> = (0..layer_count)
            .map(|index| {
                DescriptorBuilder::default()
                    .media_type(MediaType::ImageLayerGzip)
                    .digest(sha256_digest(format!("blob-{index}").as_bytes()))
                    .size(100 + index as u64)
                    .build()
                    .unwrap()
            })
            .collect();
        let history: Option<Vec<History>> = with_history.then(|| {
            (0..layer_count)
                .map(|index| {
                    HistoryBuilder::default()
                        .created("1970-01-01T00:00:00Z")
                        .created_by(format!("base layer {index}"))
                        .build()
                        .unwrap()
                })
                .collect()
        });
        let rootfs = RootFsBuilder::default().diff_ids(diff_ids).build().unwrap();
        let mut config = ImageConfigurationBuilder::default()
            .architecture(Arch::Amd64)
            .os(Os::Linux)
            .rootfs(rootfs);
        if let Some(history) = history {
            config = config.history(history);
        }
        let config = config.build().unwrap();
        Image {
            source: Some(ImageSource {
                reference: "example.test/base:latest".parse().unwrap(),
                manifest_digest: sha256_digest(b"base-manifest"),
            }),
            config,
            layers,
        }
    }

    fn layer(name: &str, contents: &[u8]) -> Layer {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(name);
        std::fs::write(&path, contents).unwrap();
        let spec = CopySpec::parse(&format!("{}:/app/{name}", path.display())).unwrap();
        build_copy_layer(&spec).unwrap()
    }

    fn options(base: Image, new_layers: Vec<Layer>) -> AssembleOptions {
        AssembleOptions {
            base,
            new_layers,
            entrypoint: None,
            cmd: None,
            labels: BTreeMap::new(),
            ports: vec![],
            workdir: None,
            user: None,
            creation_time: None,
            keep_cmd: false,
        }
    }

    #[test]
    fn appends_layers_and_history_in_order() {
        let first = layer("first", b"1");
        let second = layer("second", b"2");
        let first_diff = first.diff_id.to_string();
        let second_diff = second.diff_id.to_string();
        let bundle = assemble(options(base(2, true), vec![first, second])).unwrap();

        assert_eq!(
            &bundle.config.rootfs().diff_ids()[2..],
            &[first_diff, second_diff]
        );
        assert_eq!(bundle.config.history().as_ref().unwrap().len(), 4);
        assert!(matches!(bundle.layers[0], LayerSource::FromBase { .. }));
        assert!(matches!(bundle.layers[3], LayerSource::Built(_)));
        let manifest: oci_spec::image::ImageManifest =
            serde_json::from_slice(&bundle.manifest_bytes).unwrap();
        assert_eq!(manifest.layers().len(), 4);
    }

    #[test]
    fn base_without_history_remains_without_history() {
        let bundle = assemble(options(base(0, false), vec![layer("payload", b"x")])).unwrap();
        assert!(bundle.config.history().is_none());
    }

    #[test]
    fn rejects_base_with_mismatched_diff_ids_and_layers() {
        let mut malformed = base(1, false);
        malformed
            .config
            .rootfs_mut()
            .set_diff_ids(Vec::<String>::new());

        let error = assemble(options(malformed, vec![])).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("diff_ids count does not match manifest layers (0 != 1)"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn runtime_metadata_is_merged() {
        let mut base = base(0, true);
        let runtime = ConfigBuilder::default()
            .entrypoint(vec!["/bin/base".to_string()])
            .cmd(vec!["old-arg".to_string()])
            .labels(HashMap::from([
                ("keep".to_string(), "base".to_string()),
                ("override".to_string(), "old".to_string()),
            ]))
            .exposed_ports(vec!["8080/tcp".to_string()])
            .build()
            .unwrap();
        base.config.set_config(Some(runtime));

        let mut options = options(base, vec![]);
        options.entrypoint = Some(vec!["/app/server".to_string()]);
        options.labels = BTreeMap::from([
            ("override".to_string(), "new".to_string()),
            ("added".to_string(), "yes".to_string()),
        ]);
        options.ports = vec!["8080/tcp".to_string(), "9090/tcp".to_string()];
        options.workdir = Some("/srv".to_string());
        options.user = Some("1000:1000".to_string());
        let bundle = assemble(options).unwrap();
        let runtime = bundle.config.config().as_ref().unwrap();

        assert_eq!(
            runtime.entrypoint().as_ref().unwrap(),
            &vec!["/app/server".to_string()]
        );
        assert!(runtime.cmd().is_none());
        let labels = runtime.labels().as_ref().unwrap();
        assert_eq!(labels.get("keep").unwrap(), "base");
        assert_eq!(labels.get("override").unwrap(), "new");
        assert_eq!(labels.get("added").unwrap(), "yes");
        assert_eq!(
            runtime.exposed_ports().as_ref().unwrap(),
            &vec!["8080/tcp".to_string(), "9090/tcp".to_string()]
        );
        assert_eq!(runtime.working_dir().as_deref(), Some("/srv"));
        assert_eq!(runtime.user().as_deref(), Some("1000:1000"));
    }

    #[test]
    fn keep_cmd_preserves_base_command() {
        let mut base = base(0, true);
        base.config.set_config(Some(
            ConfigBuilder::default()
                .cmd(vec!["--default".to_string()])
                .build()
                .unwrap(),
        ));
        let mut options = options(base, vec![]);
        options.entrypoint = Some(vec!["/app/server".to_string()]);
        options.keep_cmd = true;
        let bundle = assemble(options).unwrap();
        assert_eq!(
            bundle.config.config().as_ref().unwrap().cmd().as_ref(),
            Some(&vec!["--default".to_string()])
        );
    }

    #[test]
    fn cmd_without_entrypoint_overrides_base_cmd() {
        let mut base = base(0, true);
        base.config.set_config(Some(
            ConfigBuilder::default()
                .entrypoint(vec!["/bin/base".to_string()])
                .cmd(vec!["--old".to_string()])
                .build()
                .unwrap(),
        ));
        let mut options = options(base, vec![]);
        options.cmd = Some(vec!["--new".to_string()]);
        let bundle = assemble(options).unwrap();
        let runtime = bundle.config.config().as_ref().unwrap();

        assert_eq!(runtime.cmd().as_ref(), Some(&vec!["--new".to_string()]));
        assert_eq!(
            runtime.entrypoint().as_ref(),
            Some(&vec!["/bin/base".to_string()])
        );
    }

    #[test]
    fn base_cmd_survives_when_nothing_is_overridden() {
        let mut base = base(0, true);
        base.config.set_config(Some(
            ConfigBuilder::default()
                .cmd(vec!["--default".to_string()])
                .build()
                .unwrap(),
        ));
        let bundle = assemble(options(base, vec![])).unwrap();
        assert_eq!(
            bundle.config.config().as_ref().unwrap().cmd().as_ref(),
            Some(&vec!["--default".to_string()])
        );
    }

    #[test]
    fn manifest_digest_addresses_annotated_bytes() {
        let bundle = assemble(options(base(0, true), vec![])).unwrap();
        let manifest: oci_spec::image::ImageManifest =
            serde_json::from_slice(&bundle.manifest_bytes).unwrap();
        assert_eq!(
            sha256_digest(&bundle.manifest_bytes),
            bundle.manifest_digest
        );
        assert_eq!(
            manifest.annotations().as_ref().unwrap()[ANNOTATION_BASE_IMAGE_NAME],
            "example.test/base:latest"
        );
        assert_eq!(manifest.config().digest(), &bundle.config_digest);
        assert_eq!(manifest.config().size(), bundle.config_bytes.len() as u64);
    }

    #[test]
    fn scratch_has_only_created_annotation_and_appends_copy_history() {
        let scratch = Image::scratch(&Platform::parse("linux/arm64/v8").unwrap()).unwrap();
        let bundle = assemble(options(scratch, vec![layer("payload", b"x")])).unwrap();
        let manifest: oci_spec::image::ImageManifest =
            serde_json::from_slice(&bundle.manifest_bytes).unwrap();
        let annotations = manifest.annotations().as_ref().unwrap();

        assert!(!annotations.contains_key(ANNOTATION_BASE_IMAGE_NAME));
        assert!(!annotations.contains_key(ANNOTATION_BASE_IMAGE_DIGEST));
        assert_eq!(
            annotations.get(ANNOTATION_CREATED).map(String::as_str),
            Some("1970-01-01T00:00:00Z")
        );
        assert_eq!(bundle.config.rootfs().diff_ids().len(), 1);
        assert_eq!(bundle.config.history().as_ref().unwrap().len(), 1);
        assert_eq!(bundle.config.os().to_string(), "linux");
        assert_eq!(bundle.config.architecture().to_string(), "arm64");
        assert_eq!(bundle.config.variant().as_deref(), Some("v8"));
        assert!(matches!(bundle.layers.as_slice(), [LayerSource::Built(_)]));
    }

    #[test]
    fn base_layers_require_a_registry_source() {
        let mut malformed = base(1, true);
        malformed.source = None;

        let error = assemble(options(malformed, vec![])).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("base image layers are missing their registry source"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn assembly_is_reproducible() {
        let first = assemble(options(base(0, true), vec![layer("payload", b"same")])).unwrap();
        let second = assemble(options(base(0, true), vec![layer("payload", b"same")])).unwrap();
        assert_eq!(first.config_bytes, second.config_bytes);
        assert_eq!(first.manifest_bytes, second.manifest_bytes);
        assert_eq!(first.manifest_digest, second.manifest_digest);
    }
}
