use std::fs;
use std::path::Path;

use serde_json::Value;

use crate::util::paths::base_config_dir;

/// Fields where the base config value should override the template (scalar semantics).
const SCALAR_FIELDS: &[&str] = &[
    "name",
    "image",
    "remoteUser",
    "shutdownAction",
    "waitFor",
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
    "postStartCommand",
    "postAttachCommand",
];

/// Fields that are arrays and should be concatenated (base appended to template).
const ARRAY_FIELDS: &[&str] = &["forwardPorts", "mounts", "runArgs"];

/// Fields that are key-value maps and should be merged (base keys override template keys).
const MAP_FIELDS: &[&str] = &["remoteEnv", "containerEnv"];

/// Fields that are feature maps (special merge: union of keys).
const FEATURE_FIELDS: &[&str] = &["features"];

/// Merge the user's base config (`~/.dev/base/devcontainer.json`) into a destination
/// devcontainer.json file. Returns `true` if a merge was performed, `false` if no
/// base config exists.
pub fn merge_base_config(dest: &Path) -> anyhow::Result<bool> {
    let base_config_path = base_config_dir().join("devcontainer.json");
    if !base_config_path.is_file() {
        return Ok(false);
    }

    let dest_config_path = dest.join(".devcontainer/devcontainer.json");
    if !dest_config_path.is_file() {
        return Ok(false);
    }

    // Read base config
    let base_raw = fs::read_to_string(&base_config_path)?;
    let base_stripped = json_comments::StripComments::new(base_raw.as_bytes());
    let base: Value = serde_json::from_reader(base_stripped)?;

    // Read dest config
    let dest_raw = fs::read_to_string(&dest_config_path)?;
    let dest_stripped = json_comments::StripComments::new(dest_raw.as_bytes());
    let mut dest_json: Value = serde_json::from_reader(dest_stripped)?;

    let base_obj = match base.as_object() {
        Some(obj) if !obj.is_empty() => obj,
        _ => return Ok(false),
    };

    let dest_obj = dest_json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;

    for (key, base_val) in base_obj {
        if SCALAR_FIELDS.contains(&key.as_str()) {
            // Base overrides template only if base sets it
            dest_obj.insert(key.clone(), base_val.clone());
        } else if FEATURE_FIELDS.contains(&key.as_str()) {
            // Union: base features added to template features
            merge_feature_map(dest_obj, key, base_val);
        } else if ARRAY_FIELDS.contains(&key.as_str()) {
            // Concatenate: base appended to template
            merge_array(dest_obj, key, base_val);
        } else if MAP_FIELDS.contains(&key.as_str()) {
            // Merge: base keys override template keys
            merge_map(dest_obj, key, base_val);
        } else {
            // Unknown fields: base wins (copy over as-is)
            dest_obj.insert(key.clone(), base_val.clone());
        }
    }

    let formatted = serde_json::to_string_pretty(&dest_json)?;
    fs::write(&dest_config_path, formatted)?;

    Ok(true)
}

/// Union feature maps: base features are added to template features.
/// If both have the same feature, base options override.
fn merge_feature_map(dest_obj: &mut serde_json::Map<String, Value>, key: &str, base_val: &Value) {
    let base_features = match base_val.as_object() {
        Some(obj) => obj,
        None => return,
    };

    let dest_features = dest_obj
        .entry(key)
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    if let Some(dest_map) = dest_features.as_object_mut() {
        for (feature_key, feature_val) in base_features {
            dest_map.insert(feature_key.clone(), feature_val.clone());
        }
    }
}

/// Concatenate arrays: base values appended to template values, skipping duplicates.
fn merge_array(dest_obj: &mut serde_json::Map<String, Value>, key: &str, base_val: &Value) {
    let base_arr = match base_val.as_array() {
        Some(arr) => arr,
        None => return,
    };

    let dest_arr = dest_obj
        .entry(key)
        .or_insert_with(|| Value::Array(Vec::new()));

    if let Some(dest_vec) = dest_arr.as_array_mut() {
        for item in base_arr {
            if !dest_vec.contains(item) {
                dest_vec.push(item.clone());
            }
        }
    }
}

/// Merge maps: base keys override template keys.
fn merge_map(dest_obj: &mut serde_json::Map<String, Value>, key: &str, base_val: &Value) {
    let base_map = match base_val.as_object() {
        Some(obj) => obj,
        None => return,
    };

    let dest_map = dest_obj
        .entry(key)
        .or_insert_with(|| Value::Object(serde_json::Map::new()));

    if let Some(dest_m) = dest_map.as_object_mut() {
        for (k, v) in base_map {
            dest_m.insert(k.clone(), v.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_merge_test(
        base_content: &str,
        dest_content: &str,
    ) -> (TempDir, TempDir, std::path::PathBuf) {
        // Set up base config
        let base_dir = TempDir::new().unwrap();
        fs::write(base_dir.path().join("devcontainer.json"), base_content).unwrap();

        // Set up dest config
        let dest_dir = TempDir::new().unwrap();
        let devcontainer_dir = dest_dir.path().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        let dest_config = devcontainer_dir.join("devcontainer.json");
        fs::write(&dest_config, dest_content).unwrap();

        (base_dir, dest_dir, dest_config)
    }

    fn merge_with_base(base_path: &Path, dest: &Path) -> anyhow::Result<bool> {
        let base_config_path = base_path.join("devcontainer.json");
        if !base_config_path.is_file() {
            return Ok(false);
        }

        let dest_config_path = dest.join(".devcontainer/devcontainer.json");
        if !dest_config_path.is_file() {
            return Ok(false);
        }

        let base_raw = fs::read_to_string(&base_config_path)?;
        let base: Value = serde_json::from_str(&base_raw)?;

        let dest_raw = fs::read_to_string(&dest_config_path)?;
        let mut dest_json: Value = serde_json::from_str(&dest_raw)?;

        let base_obj = match base.as_object() {
            Some(obj) if !obj.is_empty() => obj,
            _ => return Ok(false),
        };

        let dest_obj = dest_json.as_object_mut().unwrap();

        for (key, base_val) in base_obj {
            if SCALAR_FIELDS.contains(&key.as_str()) {
                dest_obj.insert(key.clone(), base_val.clone());
            } else if FEATURE_FIELDS.contains(&key.as_str()) {
                merge_feature_map(dest_obj, key, base_val);
            } else if ARRAY_FIELDS.contains(&key.as_str()) {
                merge_array(dest_obj, key, base_val);
            } else if MAP_FIELDS.contains(&key.as_str()) {
                merge_map(dest_obj, key, base_val);
            } else {
                dest_obj.insert(key.clone(), base_val.clone());
            }
        }

        let formatted = serde_json::to_string_pretty(&dest_json)?;
        fs::write(&dest_config_path, formatted)?;

        Ok(true)
    }

    #[test]
    fn test_merge_features_union() {
        let (base_dir, dest_dir, dest_config) = setup_merge_test(
            r#"{"features": {"ghcr.io/features/zsh": {}}}"#,
            r#"{"features": {"ghcr.io/features/node": {}}}"#,
        );

        let result = merge_with_base(base_dir.path(), dest_dir.path()).unwrap();
        assert!(result);

        let json: Value = serde_json::from_str(&fs::read_to_string(&dest_config).unwrap()).unwrap();
        let features = json["features"].as_object().unwrap();
        assert!(features.contains_key("ghcr.io/features/node"));
        assert!(features.contains_key("ghcr.io/features/zsh"));
    }

    #[test]
    fn test_merge_arrays_concatenate() {
        let (base_dir, dest_dir, dest_config) = setup_merge_test(
            r#"{"mounts": ["source=a,target=/a,type=bind"]}"#,
            r#"{"mounts": ["source=b,target=/b,type=bind"]}"#,
        );

        let result = merge_with_base(base_dir.path(), dest_dir.path()).unwrap();
        assert!(result);

        let json: Value = serde_json::from_str(&fs::read_to_string(&dest_config).unwrap()).unwrap();
        let mounts = json["mounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 2);
        assert_eq!(mounts[0], "source=b,target=/b,type=bind");
        assert_eq!(mounts[1], "source=a,target=/a,type=bind");
    }

    #[test]
    fn test_merge_maps_base_overrides() {
        let (base_dir, dest_dir, dest_config) = setup_merge_test(
            r#"{"remoteEnv": {"POSH_THEME": "/home/vscode/.config/omp/theme.omp.json", "SHARED": "base"}}"#,
            r#"{"remoteEnv": {"NODE_ENV": "development", "SHARED": "template"}}"#,
        );

        let result = merge_with_base(base_dir.path(), dest_dir.path()).unwrap();
        assert!(result);

        let json: Value = serde_json::from_str(&fs::read_to_string(&dest_config).unwrap()).unwrap();
        let env = json["remoteEnv"].as_object().unwrap();
        assert_eq!(env["NODE_ENV"], "development");
        assert_eq!(env["POSH_THEME"], "/home/vscode/.config/omp/theme.omp.json");
        assert_eq!(env["SHARED"], "base"); // base wins
    }

    #[test]
    fn test_merge_scalars_base_overrides() {
        let (base_dir, dest_dir, dest_config) = setup_merge_test(
            r#"{"remoteUser": "vscode"}"#,
            r#"{"image": "ubuntu", "remoteUser": "root"}"#,
        );

        let result = merge_with_base(base_dir.path(), dest_dir.path()).unwrap();
        assert!(result);

        let json: Value = serde_json::from_str(&fs::read_to_string(&dest_config).unwrap()).unwrap();
        assert_eq!(json["image"], "ubuntu"); // template preserved
        assert_eq!(json["remoteUser"], "vscode"); // base overrides
    }

    #[test]
    fn test_merge_no_base_config() {
        let dest_dir = TempDir::new().unwrap();
        let devcontainer_dir = dest_dir.path().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::write(
            devcontainer_dir.join("devcontainer.json"),
            r#"{"image": "ubuntu"}"#,
        )
        .unwrap();

        let base_dir = TempDir::new().unwrap();
        // No base config file created
        let result = merge_with_base(base_dir.path(), dest_dir.path()).unwrap();
        assert!(!result);
    }

    #[test]
    fn test_merge_empty_base_config() {
        let (base_dir, dest_dir, _) = setup_merge_test(r#"{}"#, r#"{"image": "ubuntu"}"#);

        let result = merge_with_base(base_dir.path(), dest_dir.path()).unwrap();
        assert!(!result); // Empty base = no-op
    }

    #[test]
    fn test_merge_forward_ports_concatenate() {
        let (base_dir, dest_dir, dest_config) = setup_merge_test(
            r#"{"forwardPorts": [9090]}"#,
            r#"{"forwardPorts": [3000, 8080]}"#,
        );

        let result = merge_with_base(base_dir.path(), dest_dir.path()).unwrap();
        assert!(result);

        let json: Value = serde_json::from_str(&fs::read_to_string(&dest_config).unwrap()).unwrap();
        let ports = json["forwardPorts"].as_array().unwrap();
        assert_eq!(ports.len(), 3);
        assert_eq!(ports[0], 3000);
        assert_eq!(ports[1], 8080);
        assert_eq!(ports[2], 9090);
    }

    #[test]
    fn test_merge_unknown_fields_base_wins() {
        let (base_dir, dest_dir, dest_config) = setup_merge_test(
            r#"{"customSetting": "from-base"}"#,
            r#"{"image": "ubuntu", "customSetting": "from-template"}"#,
        );

        let result = merge_with_base(base_dir.path(), dest_dir.path()).unwrap();
        assert!(result);

        let json: Value = serde_json::from_str(&fs::read_to_string(&dest_config).unwrap()).unwrap();
        assert_eq!(json["customSetting"], "from-base");
    }
}
