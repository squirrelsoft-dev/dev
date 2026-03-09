use futures_util::future::join_all;
use serde::{Deserialize, Serialize};

use crate::error::DevError;
use crate::oci::registry::pull_first_layer;
use super::cache::CacheManager;

const COLLECTION_INDEX_URL: &str = "https://raw.githubusercontent.com/devcontainers/devcontainers.github.io/gh-pages/_data/collection-index.yml";

// --- Public types ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Collection {
    pub name: String,
    pub oci_ref: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemplateMetadata {
    pub id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub options: Vec<OptionDef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureMetadata {
    pub id: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub options: Vec<OptionDef>,
    #[serde(default, rename = "installAfter")]
    pub install_after: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OptionDef {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, rename = "type")]
    pub option_type: String,
    #[serde(default)]
    pub default: String,
    #[serde(default, rename = "enum")]
    pub enum_values: Option<Vec<String>>,
    #[serde(default)]
    pub proposals: Option<Vec<String>>,
}

// --- Collection classification ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    Template,
    Feature,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateTier {
    Official,
    Microsoft,
    Community,
}

/// Classify a collection by whether its name contains "feature" or "template".
pub fn classify_collection(c: &Collection) -> Option<CollectionKind> {
    let lower = c.name.to_lowercase();
    if lower.contains("feature") {
        Some(CollectionKind::Feature)
    } else if lower.contains("template") {
        Some(CollectionKind::Template)
    } else {
        None
    }
}

/// Filter to template-kind collections.
pub fn template_collections(collections: &[Collection]) -> Vec<&Collection> {
    collections
        .iter()
        .filter(|c| classify_collection(c) == Some(CollectionKind::Template))
        .collect()
}

/// Filter to feature-kind collections.
pub fn feature_collections(collections: &[Collection]) -> Vec<&Collection> {
    collections
        .iter()
        .filter(|c| classify_collection(c) == Some(CollectionKind::Feature))
        .collect()
}

/// Categorize a template collection into Official, Microsoft, or Community.
pub fn template_tier(c: &Collection) -> TemplateTier {
    if c.oci_ref.starts_with("ghcr.io/devcontainers/templates") {
        TemplateTier::Official
    } else if c.oci_ref.starts_with("ghcr.io/microsoft") {
        TemplateTier::Microsoft
    } else {
        TemplateTier::Community
    }
}

/// Fetch features from all feature collections in parallel.
/// Returns each feature paired with the OCI ref of its collection.
pub async fn fetch_all_features(
    collections: &[Collection],
    force_refresh: bool,
) -> Vec<(String, FeatureMetadata)> {
    let feature_cols = feature_collections(collections);
    let fetches: Vec<_> = feature_cols
        .iter()
        .map(|c| fetch_features(c, force_refresh))
        .collect();
    let results = join_all(fetches).await;

    let mut all = Vec::new();
    for (col, result) in feature_cols.iter().zip(results) {
        if let Ok(features) = result {
            for f in features {
                all.push((col.oci_ref.clone(), f));
            }
        }
    }
    all
}

// --- Raw YAML structure for parsing the collection index ---

#[derive(Deserialize)]
struct RawCollectionEntry {
    name: String,
    #[serde(default, rename = "ociReference")]
    oci_reference: Option<String>,
}

// --- Raw JSON for devcontainer-collection.json ---

#[derive(Deserialize)]
struct DevcontainerCollectionJson {
    #[serde(default)]
    templates: Vec<RawTemplateEntry>,
    #[serde(default)]
    features: Vec<RawFeatureEntry>,
}

#[derive(Deserialize)]
struct RawTemplateEntry {
    id: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    options: serde_json::Value,
}

#[derive(Deserialize)]
struct RawFeatureEntry {
    id: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    options: serde_json::Value,
    #[serde(default, rename = "installAfter")]
    install_after: Vec<String>,
}

// --- Public API ---

/// Fetch and parse the devcontainers collection index.
pub async fn fetch_collection_index(
    force_refresh: bool,
) -> Result<Vec<Collection>, DevError> {
    let cache = CacheManager::new()?;
    let cache_key = "collection-index.yml";

    if !force_refresh && cache.is_fresh(cache_key) {
        if let Some(data) = cache.read(cache_key) {
            if let Ok(collections) = parse_collection_index(&data) {
                return Ok(collections);
            }
        }
    }

    let (data, etag) = fetch_with_etag(COLLECTION_INDEX_URL, cache.etag(cache_key)).await?;

    let data = if let Some(data) = data {
        cache.write(cache_key, &data, etag)?;
        data
    } else {
        // 304 Not Modified - use cached version
        cache.read(cache_key).ok_or_else(|| {
            DevError::Cache("got 304 but no cached data available".into())
        })?
    };

    parse_collection_index(&data)
}

/// Fetch template metadata for a collection from its OCI registry.
/// The devcontainer-collection.json at `<ociRef>:latest` may contain
/// a `templates` array, a `features` array, or both.
pub async fn fetch_templates(
    collection: &Collection,
    force_refresh: bool,
) -> Result<Vec<TemplateMetadata>, DevError> {
    let collection_json = fetch_collection_json(&collection.oci_ref, &collection.name, force_refresh).await?;
    parse_templates(&collection_json)
}

/// Fetch feature metadata for a collection from its OCI registry.
pub async fn fetch_features(
    collection: &Collection,
    force_refresh: bool,
) -> Result<Vec<FeatureMetadata>, DevError> {
    let collection_json = fetch_collection_json(&collection.oci_ref, &collection.name, force_refresh).await?;
    parse_features(&collection_json)
}

// --- Internal helpers ---

/// Fetch a URL with optional ETag for conditional GET.
/// Returns (Some(body), new_etag) on 200, or (None, None) on 304.
async fn fetch_with_etag(
    url: &str,
    etag: Option<String>,
) -> Result<(Option<Vec<u8>>, Option<String>), DevError> {
    let client = reqwest::Client::new();
    let mut req = client.get(url);
    if let Some(etag) = &etag {
        req = req.header("If-None-Match", etag);
    }
    let resp = req.send().await?;

    if resp.status() == reqwest::StatusCode::NOT_MODIFIED {
        return Ok((None, None));
    }

    let resp = resp.error_for_status().map_err(|e| {
        DevError::Registry(format!("failed to fetch {url}: {e}"))
    })?;

    let new_etag = resp
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let body = resp.bytes().await?;
    Ok((Some(body.to_vec()), new_etag))
}

/// Fetch devcontainer-collection.json from an OCI registry.
async fn fetch_collection_json(
    oci_ref: &str,
    collection_name: &str,
    force_refresh: bool,
) -> Result<Vec<u8>, DevError> {
    let cache = CacheManager::new()?;
    let cache_key = format!("{collection_name}-collection.json");

    if !force_refresh && cache.is_fresh(&cache_key) {
        if let Some(data) = cache.read(&cache_key) {
            return Ok(data);
        }
    }

    // The devcontainer-collection.json is stored as tag "latest"
    // in the OCI registry at the collection's ociReference
    let data = pull_first_layer(oci_ref, "latest").await?;
    cache.write(&cache_key, &data, None)?;
    Ok(data)
}

fn parse_collection_index(data: &[u8]) -> Result<Vec<Collection>, DevError> {
    let raw: Vec<RawCollectionEntry> = serde_yaml::from_slice(data)
        .map_err(|e| DevError::InvalidConfig(format!("failed to parse collection index: {e}")))?;

    Ok(raw
        .into_iter()
        .filter_map(|entry| {
            entry.oci_reference.map(|oci_ref| Collection {
                name: entry.name,
                oci_ref,
            })
        })
        .collect())
}

fn parse_templates(data: &[u8]) -> Result<Vec<TemplateMetadata>, DevError> {
    let json: DevcontainerCollectionJson = serde_json::from_slice(data)
        .map_err(|e| DevError::InvalidConfig(format!("failed to parse collection JSON: {e}")))?;

    Ok(json
        .templates
        .into_iter()
        .map(|t| TemplateMetadata {
            id: t.id,
            version: t.version,
            name: t.name,
            description: t.description,
            options: parse_options(&t.options),
        })
        .collect())
}

fn parse_features(data: &[u8]) -> Result<Vec<FeatureMetadata>, DevError> {
    let json: DevcontainerCollectionJson = serde_json::from_slice(data)
        .map_err(|e| DevError::InvalidConfig(format!("failed to parse collection JSON: {e}")))?;

    Ok(json
        .features
        .into_iter()
        .map(|f| FeatureMetadata {
            id: f.id,
            version: f.version,
            name: f.name,
            description: f.description,
            options: parse_options(&f.options),
            install_after: f.install_after,
        })
        .collect())
}

/// Parse the options map from the JSON value.
/// Options in devcontainer JSON are an object keyed by option ID.
fn parse_options(value: &serde_json::Value) -> Vec<OptionDef> {
    let Some(obj) = value.as_object() else {
        return Vec::new();
    };

    obj.iter()
        .map(|(id, v)| OptionDef {
            id: id.clone(),
            description: v
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string(),
            option_type: v
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("string")
                .to_string(),
            default: match v.get("default") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Bool(b)) => b.to_string(),
                Some(other) => other.to_string(),
                None => String::new(),
            },
            enum_values: v.get("enum").and_then(|e| {
                e.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
            }),
            proposals: v.get("proposals").and_then(|e| {
                e.as_array().map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
            }),
        })
        .collect()
}
