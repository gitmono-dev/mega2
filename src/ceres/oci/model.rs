use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const DOCKER_SCHEMA2_MANIFEST_MEDIA_TYPE: &str =
    "application/vnd.docker.distribution.manifest.v2+json";
pub const DOCKER_MANIFEST_LIST_MEDIA_TYPE: &str =
    "application/vnd.docker.distribution.manifest.list.v2+json";
pub const OCI_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub urls: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Platform {
    pub architecture: String,
    pub os: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub config: Descriptor,
    #[serde(default)]
    pub layers: Vec<Descriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestIndex {
    #[serde(rename = "schemaVersion")]
    pub schema_version: u32,
    #[serde(rename = "mediaType")]
    pub media_type: String,
    #[serde(default)]
    pub manifests: Vec<Descriptor>,
}

#[cfg(test)]
mod tests {
    use super::{
        DOCKER_MANIFEST_LIST_MEDIA_TYPE, DOCKER_SCHEMA2_MANIFEST_MEDIA_TYPE, Descriptor, Manifest,
        ManifestIndex, OCI_INDEX_MEDIA_TYPE, OCI_MANIFEST_MEDIA_TYPE,
    };

    #[test]
    fn manifest_model_roundtrip() {
        let config = Descriptor {
            media_type: "application/vnd.oci.image.config.v1+json".to_owned(),
            digest: format!("sha256:{}", "a".repeat(64)),
            size: 2,
            urls: Vec::new(),
            annotations: Default::default(),
            platform: None,
        };
        let manifest = Manifest {
            schema_version: 2,
            media_type: OCI_MANIFEST_MEDIA_TYPE.to_owned(),
            config: config.clone(),
            layers: vec![config],
        };
        let json = serde_json::to_vec(&manifest).expect("serialize manifest");
        assert_eq!(
            serde_json::from_slice::<Manifest>(&json).expect("deserialize manifest"),
            manifest
        );

        let index = ManifestIndex {
            schema_version: 2,
            media_type: OCI_INDEX_MEDIA_TYPE.to_owned(),
            manifests: Vec::new(),
        };
        assert_eq!(
            serde_json::from_str::<ManifestIndex>(
                &serde_json::to_string(&index).expect("serialize index")
            )
            .expect("deserialize index"),
            index
        );
        assert_eq!(
            DOCKER_SCHEMA2_MANIFEST_MEDIA_TYPE,
            "application/vnd.docker.distribution.manifest.v2+json"
        );
        assert_eq!(
            DOCKER_MANIFEST_LIST_MEDIA_TYPE,
            "application/vnd.docker.distribution.manifest.list.v2+json"
        );
    }
}
