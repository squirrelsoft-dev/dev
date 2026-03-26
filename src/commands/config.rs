use std::fs;
use std::path::Path;

use dialoguer::{Input, Select};
use serde_json::Value;

use crate::cli::ConfigAction;
use crate::collection::{fetch_all_features, fetch_collection_index};
use crate::tui::prompts;
use crate::util::workspace::find_devcontainer_config;

/// Entry point for `dev config` (workspace-scoped).
pub async fn run_workspace(
    workspace: &Path,
    action: Option<ConfigAction>,
    verbose: u8,
) -> anyhow::Result<()> {
    let config_path = find_devcontainer_config(workspace)?;
    run(&config_path, action, verbose).await
}

/// Core config logic, operating on a given config path.
/// Used by both workspace and global config commands.
pub async fn run(
    config_path: &Path,
    action: Option<ConfigAction>,
    verbose: u8,
) -> anyhow::Result<()> {
    match action {
        Some(ConfigAction::Set { property, value }) => config_set(config_path, &property, &value),
        Some(ConfigAction::Unset { property }) => config_unset(config_path, &property),
        Some(ConfigAction::Add { property, value }) => config_add(config_path, &property, &value),
        Some(ConfigAction::Remove { property, value }) => {
            config_remove(config_path, &property, &value)
        }
        Some(ConfigAction::List) => config_list(config_path),
        None => interactive(config_path, verbose).await,
    }
}

// --- Property type classification ---

enum PropertyType {
    Scalar,
    Array,
    KeyValueMap,
    FeatureMap,
    Lifecycle,
}

const LIFECYCLE_COMMANDS: &[&str] = &[
    "onCreateCommand",
    "updateContentCommand",
    "postCreateCommand",
    "postStartCommand",
    "postAttachCommand",
];

fn property_type(name: &str) -> PropertyType {
    match name {
        "features" => PropertyType::FeatureMap,
        "forwardPorts" => PropertyType::Array,
        "mounts" | "volumes" => PropertyType::Array,
        "remoteEnv" | "containerEnv" => PropertyType::KeyValueMap,
        n if LIFECYCLE_COMMANDS.contains(&n) => PropertyType::Lifecycle,
        _ => PropertyType::Scalar,
    }
}

// --- JSON read/write helpers ---

fn read_config(path: &Path) -> anyhow::Result<Value> {
    let raw = fs::read_to_string(path)?;
    let json: Value = crate::devcontainer::jsonc::parse_jsonc(&raw)?;
    Ok(json)
}

fn write_config(path: &Path, json: &Value) -> anyhow::Result<()> {
    let formatted = serde_json::to_string_pretty(json)?;
    fs::write(path, formatted)?;
    Ok(())
}

// --- Action handlers ---

fn config_set(path: &Path, property: &str, value: &str) -> anyhow::Result<()> {
    let mut json = read_config(path)?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;

    // Try to parse as number for numeric-looking values, otherwise store as string
    let json_value = if let Ok(n) = value.parse::<u64>() {
        Value::Number(n.into())
    } else if let Ok(b) = value.parse::<bool>() {
        Value::Bool(b)
    } else {
        Value::String(value.to_string())
    };

    obj.insert(property.to_string(), json_value);
    write_config(path, &json)?;
    println!("Set {property} = {value}");
    Ok(())
}

fn config_unset(path: &Path, property: &str) -> anyhow::Result<()> {
    let mut json = read_config(path)?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;

    if obj.remove(property).is_some() {
        write_config(path, &json)?;
        println!("Removed {property}");
    } else {
        println!("Property {property} not found");
    }
    Ok(())
}

fn config_add(path: &Path, property: &str, value: &str) -> anyhow::Result<()> {
    let mut json = read_config(path)?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;

    match property_type(property) {
        PropertyType::FeatureMap => {
            let features = obj
                .entry(property)
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(map) = features.as_object_mut() {
                map.insert(value.to_string(), serde_json::json!({}));
            }
            println!("Added feature {value}");
        }
        PropertyType::Array => {
            let arr = obj
                .entry(property)
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(vec) = arr.as_array_mut() {
                // For forwardPorts, parse as number
                if property == "forwardPorts" {
                    let port: u16 = value
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Invalid port number: {value}"))?;
                    vec.push(Value::Number(port.into()));
                    println!("Added port {port}");
                } else {
                    vec.push(Value::String(value.to_string()));
                    println!("Added {value} to {property}");
                }
            }
        }
        PropertyType::KeyValueMap => {
            let (key, val) = value
                .split_once('=')
                .ok_or_else(|| anyhow::anyhow!("Expected KEY=VALUE format, got: {value}"))?;
            let map = obj
                .entry(property)
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(m) = map.as_object_mut() {
                m.insert(key.to_string(), Value::String(val.to_string()));
            }
            println!("Set {property}.{key} = {val}");
        }
        PropertyType::Lifecycle => {
            // add supports label=command to build/merge into object form
            let (label, cmd) = value
                .split_once('=')
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Expected LABEL=COMMAND format for lifecycle add, got: {value}\n\
                         Use 'set' for a simple string command, or 'add' with label=command for the object form"
                    )
                })?;

            let entry = obj
                .entry(property)
                .or_insert_with(|| Value::Object(serde_json::Map::new()));

            // If the existing value is a string or array, convert to object form
            match entry {
                Value::String(s) => {
                    let mut map = serde_json::Map::new();
                    map.insert("default".to_string(), Value::String(s.clone()));
                    map.insert(label.to_string(), Value::String(cmd.to_string()));
                    *entry = Value::Object(map);
                }
                Value::Array(a) => {
                    let mut map = serde_json::Map::new();
                    map.insert("default".to_string(), Value::Array(a.clone()));
                    map.insert(label.to_string(), Value::String(cmd.to_string()));
                    *entry = Value::Object(map);
                }
                Value::Object(map) => {
                    map.insert(label.to_string(), Value::String(cmd.to_string()));
                }
                _ => {
                    let mut map = serde_json::Map::new();
                    map.insert(label.to_string(), Value::String(cmd.to_string()));
                    *entry = Value::Object(map);
                }
            }
            println!("Added {property}[{label}] = {cmd}");
        }
        PropertyType::Scalar => {
            anyhow::bail!(
                "Property '{property}' is scalar — use 'set' instead of 'add'"
            );
        }
    }

    write_config(path, &json)?;
    Ok(())
}

fn config_remove(path: &Path, property: &str, value: &str) -> anyhow::Result<()> {
    let mut json = read_config(path)?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;

    match property_type(property) {
        PropertyType::FeatureMap => {
            if let Some(features) = obj.get_mut(property).and_then(|v| v.as_object_mut()) {
                if features.remove(value).is_some() {
                    println!("Removed feature {value}");
                } else {
                    println!("Feature {value} not found");
                    return Ok(());
                }
            } else {
                println!("No {property} configured");
                return Ok(());
            }
        }
        PropertyType::Array => {
            if let Some(arr) = obj.get_mut(property).and_then(|v| v.as_array_mut()) {
                let before = arr.len();
                if property == "forwardPorts" {
                    let port: u16 = value
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Invalid port number: {value}"))?;
                    arr.retain(|v| v.as_u64() != Some(port as u64));
                } else {
                    arr.retain(|v| v.as_str() != Some(value));
                }
                if arr.len() < before {
                    println!("Removed {value} from {property}");
                } else {
                    println!("{value} not found in {property}");
                    return Ok(());
                }
            } else {
                println!("No {property} configured");
                return Ok(());
            }
        }
        PropertyType::KeyValueMap => {
            if let Some(map) = obj.get_mut(property).and_then(|v| v.as_object_mut()) {
                if map.remove(value).is_some() {
                    println!("Removed {value} from {property}");
                } else {
                    println!("{value} not found in {property}");
                    return Ok(());
                }
            } else {
                println!("No {property} configured");
                return Ok(());
            }
        }
        PropertyType::Lifecycle => {
            // remove a label from the object form
            if let Some(map) = obj.get_mut(property).and_then(|v| v.as_object_mut()) {
                if map.remove(value).is_some() {
                    // If only one entry remains, simplify back to scalar
                    if map.len() == 1 {
                        if let Some((_, single_val)) = map.iter().next() {
                            let simplified = single_val.clone();
                            obj.insert(property.to_string(), simplified);
                        }
                    }
                    println!("Removed {value} from {property}");
                } else {
                    println!("Label '{value}' not found in {property}");
                    return Ok(());
                }
            } else {
                anyhow::bail!(
                    "{property} is not in object form — use 'unset' to remove it entirely"
                );
            }
        }
        PropertyType::Scalar => {
            anyhow::bail!(
                "Property '{property}' is scalar — use 'unset' instead of 'remove'"
            );
        }
    }

    write_config(path, &json)?;
    Ok(())
}

fn config_list(path: &Path) -> anyhow::Result<()> {
    let json = read_config(path)?;
    let obj = json
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;

    let scalar_keys = [
        "name",
        "image",
        "remoteUser",
        "shutdownAction",
        "waitFor",
    ];
    // Scalars
    for key in &scalar_keys {
        if let Some(val) = obj.get(*key) {
            println!("{key}: {}", format_value(val));
        }
    }

    // Features
    if let Some(features) = obj.get("features").and_then(|v| v.as_object()) {
        if !features.is_empty() {
            println!("features:");
            for key in features.keys() {
                println!("  - {key}");
            }
        }
    }

    // Forward ports
    if let Some(ports) = obj.get("forwardPorts").and_then(|v| v.as_array()) {
        if !ports.is_empty() {
            let ports_str: Vec<String> = ports.iter().map(|v| format_value(v)).collect();
            println!("forwardPorts: {}", ports_str.join(", "));
        }
    }

    // Env maps
    for key in ["remoteEnv", "containerEnv"] {
        if let Some(env) = obj.get(key).and_then(|v| v.as_object()) {
            if !env.is_empty() {
                println!("{key}:");
                for (k, v) in env {
                    println!("  {k}={}", format_value(v));
                }
            }
        }
    }

    // Mounts
    if let Some(mounts) = obj.get("mounts").and_then(|v| v.as_array()) {
        if !mounts.is_empty() {
            println!("mounts:");
            for m in mounts {
                println!("  - {}", format_value(m));
            }
        }
    }

    // Lifecycle commands
    for key in LIFECYCLE_COMMANDS {
        if let Some(val) = obj.get(*key) {
            match val {
                Value::String(s) => println!("{key}: {s}"),
                Value::Array(arr) => {
                    let cmds: Vec<String> = arr.iter().map(|v| format_value(v)).collect();
                    println!("{key}: [{}]", cmds.join(", "));
                }
                Value::Object(map) => {
                    println!("{key}:");
                    for (label, cmd) in map {
                        match cmd {
                            Value::String(s) => println!("  {label}: {s}"),
                            Value::Array(arr) => {
                                let cmds: Vec<String> =
                                    arr.iter().map(|v| format_value(v)).collect();
                                println!("  {label}: [{}]", cmds.join(", "));
                            }
                            other => println!("  {label}: {other}"),
                        }
                    }
                }
                other => println!("{key}: {}", format_value(other)),
            }
        }
    }

    Ok(())
}

fn format_value(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

// --- Interactive menu ---

async fn interactive(config_path: &Path, verbose: u8) -> anyhow::Result<()> {
    let categories = [
        "Image & base setup",
        "Features",
        "Forwarded ports",
        "Environment variables",
        "Mounts",
        "Lifecycle commands",
        "Other settings",
        "Show current config",
    ];

    let selection = Select::new()
        .with_prompt("What would you like to configure?")
        .items(&categories)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    match selection {
        0 => interactive_image(config_path)?,
        1 => interactive_features(config_path, verbose).await?,
        2 => interactive_ports(config_path)?,
        3 => interactive_env(config_path)?,
        4 => interactive_mounts(config_path)?,
        5 => interactive_lifecycle(config_path)?,
        6 => interactive_other(config_path)?,
        7 => config_list(config_path)?,
        _ => unreachable!(),
    }

    Ok(())
}

fn interactive_image(config_path: &Path) -> anyhow::Result<()> {
    let json = read_config(config_path)?;
    let current = json
        .get("image")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let prompt = if current.is_empty() {
        "Container image".to_string()
    } else {
        format!("Container image (current: {current})")
    };

    let value: String = Input::new()
        .with_prompt(&prompt)
        .allow_empty(true)
        .interact_text()?;

    if value.is_empty() {
        if !current.is_empty() {
            config_unset(config_path, "image")?;
        }
    } else {
        config_set(config_path, "image", &value)?;
    }
    Ok(())
}

async fn interactive_features(config_path: &Path, _verbose: u8) -> anyhow::Result<()> {
    let actions = ["Add features", "Remove features"];
    let selection = Select::new()
        .with_prompt("Action")
        .items(&actions)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    match selection {
        0 => {
            // Add features — reuse multi_select_features
            eprintln!("Fetching feature catalog...");
            let collections = fetch_collection_index(false).await?;
            let features = fetch_all_features(&collections, false).await;

            if features.is_empty() {
                println!("No features available.");
                return Ok(());
            }

            // Get currently selected features for pre-selection
            let json = read_config(config_path)?;
            let preselected: Vec<String> = json
                .get("features")
                .and_then(|v| v.as_object())
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();

            let selected = prompts::multi_select_features(&features, &preselected)?;

            // Re-read and write all features
            let mut json = read_config(config_path)?;
            let obj = json
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;

            let features_val = obj
                .entry("features")
                .or_insert_with(|| Value::Object(serde_json::Map::new()));

            if let Some(fmap) = features_val.as_object_mut() {
                // Add newly selected features (keep existing ones)
                for feature_ref in &selected {
                    fmap.entry(feature_ref.clone())
                        .or_insert_with(|| serde_json::json!({}));
                }
            }

            write_config(config_path, &json)?;
            println!("Features updated.");
        }
        1 => {
            // Remove features
            let json = read_config(config_path)?;
            let features: Vec<String> = json
                .get("features")
                .and_then(|v| v.as_object())
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();

            if features.is_empty() {
                println!("No features configured.");
                return Ok(());
            }

            let dims = crate::tui::term_dimensions();
            let display: Vec<String> = features
                .iter()
                .map(|f| crate::tui::truncate_to_width(f, dims.max_width))
                .collect();
            let selections = dialoguer::MultiSelect::new()
                .with_prompt("Select features to remove (space to toggle, enter to confirm)")
                .items(&display)
                .max_length(dims.max_length)
                .interact_opt()?
                .unwrap_or_default();

            for idx in selections {
                config_remove(config_path, "features", &features[idx])?;
            }
        }
        _ => unreachable!(),
    }

    Ok(())
}

fn interactive_ports(config_path: &Path) -> anyhow::Result<()> {
    let actions = ["Add port", "Remove port"];
    let selection = Select::new()
        .with_prompt("Action")
        .items(&actions)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    match selection {
        0 => {
            let port: String = Input::new()
                .with_prompt("Port number")
                .interact_text()?;
            config_add(config_path, "forwardPorts", &port)?;
        }
        1 => {
            let json = read_config(config_path)?;
            let ports: Vec<String> = json
                .get("forwardPorts")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(|v| format_value(v)).collect())
                .unwrap_or_default();

            if ports.is_empty() {
                println!("No forwarded ports configured.");
                return Ok(());
            }

            let sel = Select::new()
                .with_prompt("Select port to remove")
                .items(&ports)
                .interact_opt()?
                .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

            config_remove(config_path, "forwardPorts", &ports[sel])?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn interactive_env(config_path: &Path) -> anyhow::Result<()> {
    let env_types = ["remoteEnv", "containerEnv"];
    let env_sel = Select::new()
        .with_prompt("Which environment?")
        .items(&env_types)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    let env_key = env_types[env_sel];

    let actions = ["Add variable", "Remove variable"];
    let action_sel = Select::new()
        .with_prompt("Action")
        .items(&actions)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    match action_sel {
        0 => {
            let kv: String = Input::new()
                .with_prompt("KEY=VALUE")
                .interact_text()?;
            config_add(config_path, env_key, &kv)?;
        }
        1 => {
            let json = read_config(config_path)?;
            let keys: Vec<String> = json
                .get(env_key)
                .and_then(|v| v.as_object())
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();

            if keys.is_empty() {
                println!("No {env_key} variables configured.");
                return Ok(());
            }

            let sel = Select::new()
                .with_prompt("Select variable to remove")
                .items(&keys)
                .interact_opt()?
                .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

            config_remove(config_path, env_key, &keys[sel])?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn interactive_mounts(config_path: &Path) -> anyhow::Result<()> {
    let actions = ["Add mount", "Remove mount"];
    let selection = Select::new()
        .with_prompt("Action")
        .items(&actions)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    match selection {
        0 => {
            let mount: String = Input::new()
                .with_prompt("Mount string (e.g. source=mydata,target=/data,type=volume)")
                .interact_text()?;
            config_add(config_path, "mounts", &mount)?;
        }
        1 => {
            let json = read_config(config_path)?;
            let mounts: Vec<String> = json
                .get("mounts")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().map(|v| format_value(v)).collect())
                .unwrap_or_default();

            if mounts.is_empty() {
                println!("No mounts configured.");
                return Ok(());
            }

            let sel = Select::new()
                .with_prompt("Select mount to remove")
                .items(&mounts)
                .interact_opt()?
                .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

            config_remove(config_path, "mounts", &mounts[sel])?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn interactive_lifecycle(config_path: &Path) -> anyhow::Result<()> {
    let sel = Select::new()
        .with_prompt("Which lifecycle command?")
        .items(LIFECYCLE_COMMANDS)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    let key = LIFECYCLE_COMMANDS[sel];

    let json = read_config(config_path)?;
    let current = json.get(key);

    // Show current value
    let current_desc = match current {
        Some(Value::String(s)) => format!("string: {s}"),
        Some(Value::Array(a)) => {
            let cmds: Vec<String> = a.iter().map(|v| format_value(v)).collect();
            format!("array: [{}]", cmds.join(", "))
        }
        Some(Value::Object(map)) => {
            let labels: Vec<&String> = map.keys().collect();
            format!("object with labels: {}", labels.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
        }
        _ => "not set".to_string(),
    };

    println!("Current {key}: {current_desc}");

    let actions = [
        "Set as simple command (string)",
        "Add labeled command (object form)",
        "Remove labeled command",
        "Remove entirely",
    ];

    let action_sel = Select::new()
        .with_prompt("Action")
        .items(&actions)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    match action_sel {
        0 => {
            let value: String = Input::new()
                .with_prompt("Command")
                .interact_text()?;
            config_set(config_path, key, &value)?;
        }
        1 => {
            let label: String = Input::new()
                .with_prompt("Label (e.g. build, test, install)")
                .interact_text()?;
            let cmd: String = Input::new()
                .with_prompt("Command")
                .interact_text()?;
            config_add(config_path, key, &format!("{label}={cmd}"))?;
        }
        2 => {
            // Show labels to pick from
            let json = read_config(config_path)?;
            if let Some(map) = json.get(key).and_then(|v| v.as_object()) {
                let labels: Vec<String> = map.keys().cloned().collect();
                if labels.is_empty() {
                    println!("{key} is not in object form.");
                    return Ok(());
                }
                let label_sel = Select::new()
                    .with_prompt("Select label to remove")
                    .items(&labels)
                    .interact_opt()?
                    .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;
                config_remove(config_path, key, &labels[label_sel])?;
            } else {
                println!("{key} is not in object form — use 'Remove entirely' instead.");
            }
        }
        3 => {
            config_unset(config_path, key)?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn interactive_other(config_path: &Path) -> anyhow::Result<()> {
    let properties = ["remoteUser", "shutdownAction", "waitFor"];

    let sel = Select::new()
        .with_prompt("Which property?")
        .items(&properties)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    let key = properties[sel];

    let json = read_config(config_path)?;
    let current = json
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let prompt = if current.is_empty() {
        key.to_string()
    } else {
        format!("{key} (current: {current}, leave empty to remove)")
    };

    let value: String = Input::new()
        .with_prompt(&prompt)
        .allow_empty(true)
        .interact_text()?;

    if value.is_empty() {
        if !current.is_empty() {
            config_unset(config_path, key)?;
        }
    } else {
        config_set(config_path, key, &value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn setup_config(content: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("devcontainer.json");
        fs::write(&config_path, content).unwrap();
        (dir, config_path)
    }

    #[test]
    fn test_config_set_string() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        config_set(&path, "image", "alpine").unwrap();
        let json = read_config(&path).unwrap();
        assert_eq!(json["image"], "alpine");
    }

    #[test]
    fn test_config_set_number() {
        let (_dir, path) = setup_config(r#"{}"#);
        config_set(&path, "shutdownAction", "none").unwrap();
        let json = read_config(&path).unwrap();
        assert_eq!(json["shutdownAction"], "none");
    }

    #[test]
    fn test_config_set_bool() {
        let (_dir, path) = setup_config(r#"{}"#);
        config_set(&path, "updateRemoteUserUID", "true").unwrap();
        let json = read_config(&path).unwrap();
        assert_eq!(json["updateRemoteUserUID"], true);
    }

    #[test]
    fn test_config_unset() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu", "remoteUser": "vscode"}"#);
        config_unset(&path, "image").unwrap();
        let json = read_config(&path).unwrap();
        assert!(json.get("image").is_none());
        assert_eq!(json["remoteUser"], "vscode");
    }

    #[test]
    fn test_config_unset_missing() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        // Should not error, just print "not found"
        config_unset(&path, "nonexistent").unwrap();
    }

    #[test]
    fn test_config_add_feature() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        config_add(&path, "features", "ghcr.io/devcontainers/features/node").unwrap();
        let json = read_config(&path).unwrap();
        assert!(json["features"]["ghcr.io/devcontainers/features/node"].is_object());
    }

    #[test]
    fn test_config_add_port() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        config_add(&path, "forwardPorts", "3000").unwrap();
        config_add(&path, "forwardPorts", "8080").unwrap();
        let json = read_config(&path).unwrap();
        let ports = json["forwardPorts"].as_array().unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0], 3000);
        assert_eq!(ports[1], 8080);
    }

    #[test]
    fn test_config_add_env() {
        let (_dir, path) = setup_config(r#"{}"#);
        config_add(&path, "remoteEnv", "NODE_ENV=development").unwrap();
        let json = read_config(&path).unwrap();
        assert_eq!(json["remoteEnv"]["NODE_ENV"], "development");
    }

    #[test]
    fn test_config_add_mount() {
        let (_dir, path) = setup_config(r#"{}"#);
        config_add(&path, "mounts", "source=mydata,target=/data,type=volume").unwrap();
        let json = read_config(&path).unwrap();
        let mounts = json["mounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0], "source=mydata,target=/data,type=volume");
    }

    #[test]
    fn test_config_remove_feature() {
        let (_dir, path) = setup_config(
            r#"{"features": {"ghcr.io/devcontainers/features/node": {}, "ghcr.io/devcontainers/features/python": {}}}"#,
        );
        config_remove(&path, "features", "ghcr.io/devcontainers/features/node").unwrap();
        let json = read_config(&path).unwrap();
        let features = json["features"].as_object().unwrap();
        assert_eq!(features.len(), 1);
        assert!(features.contains_key("ghcr.io/devcontainers/features/python"));
    }

    #[test]
    fn test_config_remove_port() {
        let (_dir, path) = setup_config(r#"{"forwardPorts": [3000, 8080]}"#);
        config_remove(&path, "forwardPorts", "3000").unwrap();
        let json = read_config(&path).unwrap();
        let ports = json["forwardPorts"].as_array().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0], 8080);
    }

    #[test]
    fn test_config_remove_env() {
        let (_dir, path) =
            setup_config(r#"{"remoteEnv": {"NODE_ENV": "development", "DEBUG": "1"}}"#);
        config_remove(&path, "remoteEnv", "NODE_ENV").unwrap();
        let json = read_config(&path).unwrap();
        let env = json["remoteEnv"].as_object().unwrap();
        assert_eq!(env.len(), 1);
        assert!(env.contains_key("DEBUG"));
    }

    #[test]
    fn test_config_add_lifecycle_creates_object() {
        let (_dir, path) = setup_config(r#"{}"#);
        config_add(&path, "postCreateCommand", "build=npm run build").unwrap();
        let json = read_config(&path).unwrap();
        let obj = json["postCreateCommand"].as_object().unwrap();
        assert_eq!(obj["build"], "npm run build");
    }

    #[test]
    fn test_config_add_lifecycle_merges_into_existing_object() {
        let (_dir, path) =
            setup_config(r#"{"postCreateCommand": {"build": "npm run build"}}"#);
        config_add(&path, "postCreateCommand", "test=npm test").unwrap();
        let json = read_config(&path).unwrap();
        let obj = json["postCreateCommand"].as_object().unwrap();
        assert_eq!(obj["build"], "npm run build");
        assert_eq!(obj["test"], "npm test");
    }

    #[test]
    fn test_config_add_lifecycle_converts_string_to_object() {
        let (_dir, path) = setup_config(r#"{"postCreateCommand": "npm install"}"#);
        config_add(&path, "postCreateCommand", "build=npm run build").unwrap();
        let json = read_config(&path).unwrap();
        let obj = json["postCreateCommand"].as_object().unwrap();
        // Original string moved under "default" key
        assert_eq!(obj["default"], "npm install");
        assert_eq!(obj["build"], "npm run build");
    }

    #[test]
    fn test_config_add_lifecycle_converts_array_to_object() {
        let (_dir, path) =
            setup_config(r#"{"postCreateCommand": ["npm install", "npm run build"]}"#);
        config_add(&path, "postCreateCommand", "test=npm test").unwrap();
        let json = read_config(&path).unwrap();
        let obj = json["postCreateCommand"].as_object().unwrap();
        // Original array moved under "default" key
        assert!(obj["default"].is_array());
        assert_eq!(obj["test"], "npm test");
    }

    #[test]
    fn test_config_remove_lifecycle_label() {
        let (_dir, path) = setup_config(
            r#"{"postCreateCommand": {"build": "npm run build", "test": "npm test", "lint": "npm run lint"}}"#,
        );
        config_remove(&path, "postCreateCommand", "test").unwrap();
        let json = read_config(&path).unwrap();
        let obj = json["postCreateCommand"].as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert!(!obj.contains_key("test"));
    }

    #[test]
    fn test_config_remove_lifecycle_simplifies_single_entry() {
        let (_dir, path) = setup_config(
            r#"{"postCreateCommand": {"build": "npm run build", "test": "npm test"}}"#,
        );
        config_remove(&path, "postCreateCommand", "test").unwrap();
        let json = read_config(&path).unwrap();
        // Should simplify to the remaining value directly
        assert_eq!(json["postCreateCommand"], "npm run build");
    }

    #[test]
    fn test_config_remove_lifecycle_string_errors() {
        let (_dir, path) = setup_config(r#"{"postCreateCommand": "npm install"}"#);
        let result = config_remove(&path, "postCreateCommand", "default");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_add_lifecycle_without_equals_errors() {
        let (_dir, path) = setup_config(r#"{}"#);
        let result = config_add(&path, "postCreateCommand", "npm install");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_set_lifecycle_as_string() {
        // set still works as simple string
        let (_dir, path) = setup_config(r#"{}"#);
        config_set(&path, "postCreateCommand", "npm install").unwrap();
        let json = read_config(&path).unwrap();
        assert_eq!(json["postCreateCommand"], "npm install");
    }

    #[test]
    fn test_config_list_lifecycle_object() {
        let (_dir, path) = setup_config(
            r#"{"postCreateCommand": {"build": "npm run build", "test": "npm test"}}"#,
        );
        // Just verify it doesn't error
        config_list(&path).unwrap();
    }

    #[test]
    fn test_config_list_lifecycle_array() {
        let (_dir, path) =
            setup_config(r#"{"postCreateCommand": ["npm install", "npm run build"]}"#);
        config_list(&path).unwrap();
    }

    #[test]
    fn test_config_add_scalar_errors() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        let result = config_add(&path, "image", "alpine");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_remove_scalar_errors() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        let result = config_remove(&path, "image", "ubuntu");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_list() {
        let (_dir, path) = setup_config(
            r#"{
                "image": "ubuntu",
                "remoteUser": "vscode",
                "features": {"ghcr.io/devcontainers/features/node": {}},
                "forwardPorts": [3000, 8080],
                "remoteEnv": {"NODE_ENV": "development"},
                "postCreateCommand": "npm install"
            }"#,
        );
        // Just verify it doesn't error
        config_list(&path).unwrap();
    }
}
