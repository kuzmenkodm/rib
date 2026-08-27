pub mod assemble;
pub mod layer;

use anyhow::{Result, bail};
use oci_client::Reference;
use oci_spec::image::{
    Arch, Descriptor, Digest, ImageConfiguration, ImageConfigurationBuilder, Os, RootFsBuilder,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    pub os: String,
    pub architecture: String,
    pub variant: Option<String>,
}

impl Platform {
    /// Parse "<os>/<arch>" or "<os>/<arch>/<variant>" (e.g. "linux/arm64/v8")
    pub fn parse(s: &str) -> Result<Self> {
        let parts: Vec<&str> = s.split('/').collect();
        let (os, arch, variant) = match parts.as_slice() {
            [os, arch] => (*os, *arch, None),
            [os, arch, var] => (*os, *arch, Some(var.to_string())),
            _ => bail!("bad platform, expected <os>/<arch>[/<variant>]: {s}"),
        };
        if os.is_empty() || arch.is_empty() {
            bail!("bad platform, empty os or architecture: {s}");
        }
        Ok(Self {
            os: os.to_string(),
            architecture: arch.to_string(),
            variant,
        })
    }

    /// True iff this platform matches a manifest-list descriptor or config
    pub fn matches(&self, os: &str, arch: &str, variant: Option<&str>) -> bool {
        if self.os != os || self.architecture != arch {
            return false;
        }
        match &self.variant {
            Some(requested) => variant == Some(requested.as_str()),
            None => true,
        }
    }
}

impl std::fmt::Display for Platform {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.os, self.architecture)?;
        if let Some(v) = &self.variant {
            write!(f, "/{v}")?;
        }
        Ok(())
    }
}

/// Base image metadata used for assembly. Layer bytes are intentionally absent
#[derive(Debug, Clone)]
pub struct Image {
    /// The registry source, or `None` for the special empty `scratch` base
    pub source: Option<ImageSource>,
    /// Parsed image config for patching
    pub config: ImageConfiguration,
    /// Layer descriptors as found in the manifest, in order
    pub layers: Vec<Descriptor>,
}

impl Image {
    /// Construct the empty OCI image configuration represented by `FROM scratch`
    pub fn scratch(platform: &Platform) -> Result<Self> {
        let rootfs = RootFsBuilder::default()
            .diff_ids(Vec::<String>::new())
            .build()?;
        let mut configuration = ImageConfigurationBuilder::default()
            .architecture(Arch::from(platform.architecture.as_str()))
            .os(Os::from(platform.os.as_str()))
            .rootfs(rootfs)
            .history(Vec::new());
        if let Some(variant) = &platform.variant {
            configuration = configuration.variant(variant.clone());
        }
        let config = configuration.build()?;

        Ok(Self {
            source: None,
            config,
            layers: Vec::new(),
        })
    }
}

/// Where a base image was pulled from. Used by archive writers — registry push
/// can cross-mount blobs from the same source repo for free
#[derive(Debug, Clone)]
pub struct ImageSource {
    pub reference: Reference,
    pub manifest_digest: Digest,
}

impl ImageSource {
    pub fn repository_reference(&self) -> Reference {
        format!(
            "{}/{}",
            self.reference.resolve_registry(),
            self.reference.repository()
        )
        .parse()
        .expect("parts of a parsed reference form a valid repository reference")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_parse_and_display() {
        let basic = Platform::parse("linux/amd64").unwrap();
        assert_eq!(basic.os, "linux");
        assert_eq!(basic.architecture, "amd64");
        assert_eq!(basic.variant, None);
        assert_eq!(basic.to_string(), "linux/amd64");

        let variant = Platform::parse("linux/arm64/v8").unwrap();
        assert_eq!(variant.variant.as_deref(), Some("v8"));
        assert_eq!(variant.to_string(), "linux/arm64/v8");
    }

    #[test]
    fn platform_rejects_malformed_values() {
        assert!(Platform::parse("just-os").is_err());
        assert!(Platform::parse("linux//amd64").is_err());
        assert!(Platform::parse("linux/amd64/v8/extra").is_err());
        assert!(Platform::parse("/amd64").is_err());
    }

    #[test]
    fn platform_matching_handles_variants() {
        let generic = Platform::parse("linux/arm64").unwrap();
        assert!(generic.matches("linux", "arm64", None));
        assert!(generic.matches("linux", "arm64", Some("v8")));
        assert!(!generic.matches("linux", "amd64", None));

        let specific = Platform::parse("linux/arm64/v8").unwrap();
        assert!(specific.matches("linux", "arm64", Some("v8")));
        assert!(!specific.matches("linux", "arm64", None));
        assert!(!specific.matches("linux", "arm64", Some("v7")));
    }

    #[test]
    fn scratch_uses_the_requested_platform_and_has_no_registry_source() {
        let scratch = Image::scratch(&Platform::parse("linux/arm64/v8").unwrap()).unwrap();

        assert!(scratch.source.is_none());
        assert!(scratch.layers.is_empty());
        assert!(scratch.config.rootfs().diff_ids().is_empty());
        assert_eq!(scratch.config.history(), &Some(Vec::new()));
        assert_eq!(scratch.config.os().to_string(), "linux");
        assert_eq!(scratch.config.architecture().to_string(), "arm64");
        assert_eq!(scratch.config.variant().as_deref(), Some("v8"));
    }
}
