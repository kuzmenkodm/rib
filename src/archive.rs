mod docker;
mod oci;
mod output;

pub use docker::write_docker_archive;
pub use oci::write_oci_archive;

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashSet};
    use std::fs::File;
    use std::io::Read;
    use std::path::Path;

    use anyhow::{Result, bail};
    use oci_spec::image::{ANNOTATION_REF_NAME, ImageIndex};
    use serde::Deserialize;
    use tempfile::TempDir;

    use super::*;
    use crate::blob::LayerProvider;
    use crate::builder::copy::CopySpec;
    use crate::builder::layer::build_copy_layer;
    use crate::digest::blob_path;
    use crate::image::assemble::{AssembleOptions, ImageBundle, assemble};
    use crate::image::layer::LayerSource;
    use crate::image::{Image, Platform};

    struct BuiltOnly;

    impl LayerProvider for BuiltOnly {
        fn blob_file(&self, source: &LayerSource) -> Result<File> {
            match source {
                LayerSource::Built(layer) => layer.open_compressed(),
                LayerSource::FromBase { .. } => bail!("fixture has no base layers"),
            }
        }

        fn uncompressed_file(&self, source: &LayerSource) -> Result<File> {
            match source {
                LayerSource::Built(layer) => layer.open_uncompressed(),
                LayerSource::FromBase { .. } => bail!("fixture has no base layers"),
            }
        }
    }

    fn bundle(contents: &[u8]) -> ImageBundle {
        let base = Image::scratch(&Platform::parse("linux/amd64").unwrap()).unwrap();
        let sources = TempDir::new().unwrap();
        let source = sources.path().join("payload");
        std::fs::write(&source, contents).unwrap();
        let layer = build_copy_layer(
            &CopySpec::parse(&format!("{}:/app/payload", source.display())).unwrap(),
        )
        .unwrap();
        assemble(AssembleOptions {
            base,
            new_layers: vec![layer],
            entrypoint: None,
            cmd: None,
            labels: BTreeMap::new(),
            ports: vec![],
            workdir: None,
            user: None,
            creation_time: None,
            keep_cmd: false,
        })
        .unwrap()
    }

    fn paths(path: &Path) -> HashSet<String> {
        ::tar::Archive::new(File::open(path).unwrap())
            .entries()
            .unwrap()
            .map(|entry| {
                entry
                    .unwrap()
                    .path()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    fn read_entry(path: &Path, name: &str) -> Vec<u8> {
        let mut archive = ::tar::Archive::new(File::open(path).unwrap());
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if entry.path().unwrap().to_string_lossy() == name {
                let mut bytes = Vec::new();
                entry.read_to_end(&mut bytes).unwrap();
                return bytes;
            }
        }
        panic!("archive contains no entry named {name}");
    }

    #[test]
    fn oci_archive_references_exact_bundle_content() {
        let bundle = bundle(b"oci");
        let directory = TempDir::new().unwrap();
        let archive = directory.path().join("image.tar");
        write_oci_archive(&bundle, &BuiltOnly, &archive, Some("app:v1")).unwrap();

        let paths = paths(&archive);
        assert!(paths.contains("oci-layout"));
        assert!(paths.contains("index.json"));
        assert!(paths.contains(&blob_path(&bundle.manifest_digest)));
        assert!(paths.contains(&blob_path(&bundle.config_digest)));
        assert_eq!(
            read_entry(&archive, &blob_path(&bundle.manifest_digest)),
            bundle.manifest_bytes.as_ref()
        );

        let layout: serde_json::Value =
            serde_json::from_slice(&read_entry(&archive, "oci-layout")).unwrap();
        assert_eq!(layout["imageLayoutVersion"], "1.0.0");
        let index: ImageIndex =
            serde_json::from_slice(&read_entry(&archive, "index.json")).unwrap();
        let descriptor = &index.manifests()[0];
        assert_eq!(descriptor.digest(), &bundle.manifest_digest);
        assert_eq!(descriptor.size(), bundle.manifest_bytes.len() as u64);
        assert_eq!(
            descriptor.annotations().as_ref().unwrap()[ANNOTATION_REF_NAME],
            "app:v1"
        );
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct DockerManifest {
        config: String,
        repo_tags: Option<Vec<String>>,
        layers: Vec<String>,
    }

    #[test]
    fn docker_archive_manifest_references_uncompressed_layers() {
        let bundle = bundle(b"docker");
        let directory = TempDir::new().unwrap();
        let archive = directory.path().join("image.tar");
        write_docker_archive(&bundle, &BuiltOnly, &archive, Some("app:v1")).unwrap();

        let manifest: Vec<DockerManifest> =
            serde_json::from_slice(&read_entry(&archive, "manifest.json")).unwrap();
        let manifest = &manifest[0];
        let paths = paths(&archive);
        assert!(paths.contains(&manifest.config));
        assert_eq!(
            manifest.repo_tags.as_ref().unwrap(),
            &vec!["app:v1".to_string()]
        );
        assert!(manifest.layers.iter().all(|layer| paths.contains(layer)));
        assert_eq!(
            read_entry(&archive, &manifest.config),
            bundle.config_bytes.as_ref()
        );

        let layer = read_entry(&archive, &manifest.layers[0]);
        assert_ne!(&layer[..2], &[0x1f, 0x8b]);
        let entries: Vec<_> = ::tar::Archive::new(layer.as_slice())
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().into_owned())
            .collect();
        assert!(entries.iter().any(|path| path.ends_with("payload")));
    }

    #[test]
    fn archives_are_reproducible() {
        let first = bundle(b"same");
        let second = bundle(b"same");
        let directory = TempDir::new().unwrap();
        let first_oci = directory.path().join("first-oci.tar");
        let second_oci = directory.path().join("second-oci.tar");
        write_oci_archive(&first, &BuiltOnly, &first_oci, Some("app:v1")).unwrap();
        write_oci_archive(&second, &BuiltOnly, &second_oci, Some("app:v1")).unwrap();
        assert_eq!(
            std::fs::read(first_oci).unwrap(),
            std::fs::read(second_oci).unwrap()
        );

        let first_docker = directory.path().join("first-docker.tar");
        let second_docker = directory.path().join("second-docker.tar");
        write_docker_archive(&first, &BuiltOnly, &first_docker, Some("app:v1")).unwrap();
        write_docker_archive(&second, &BuiltOnly, &second_docker, Some("app:v1")).unwrap();
        assert_eq!(
            std::fs::read(first_docker).unwrap(),
            std::fs::read(second_docker).unwrap()
        );
    }

    struct AlwaysFails;

    impl LayerProvider for AlwaysFails {
        fn blob_file(&self, _: &LayerSource) -> Result<File> {
            bail!("intentional failure")
        }

        fn uncompressed_file(&self, _: &LayerSource) -> Result<File> {
            bail!("intentional failure")
        }
    }

    #[test]
    fn failed_archive_preserves_existing_target() {
        let directory = TempDir::new().unwrap();
        let target = directory.path().join("image.tar");
        std::fs::write(&target, b"previous-good-output").unwrap();
        assert!(write_oci_archive(&bundle(b"x"), &AlwaysFails, &target, None).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"previous-good-output");
    }
}
