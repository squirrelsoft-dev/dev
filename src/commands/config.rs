use std::fs;
use std::path::Path;

use dialoguer::{Input, Select};
use serde_json::Value;

use crate::cli::ConfigAction;
use crate::collection::{fetch_all_features, fetch_collection_index};
use crate::devcontainer::recipe::Recipe;
use crate::tui::prompts;
use crate::util::workspace::{find_config_source, find_devcontainer_config, ConfigSource};

/// Tracks where config changes should be written.
struct ConfigTarget<'a> {
    /// The composed devcontainer.json (always written for immediate effect).
    config_path: &'a Path,
    /// If present, also persist changes to recipe.json customizations.
    recipe_path: Option<&'a Path>,
}

/// Entry point for `dev config` (workspace-scoped).
pub async fn run_workspace(
    workspace: &Path,
    action: Option<ConfigAction>,
    verbose: u8,
) -> anyhow::Result<()> {
    match find_config_source(workspace)? {
        ConfigSource::Direct(path) => run(&path, action, verbose).await,
        ConfigSource::Recipe(recipe_path) => {
            let config_path = find_devcontainer_config(workspace)?;
            let target = ConfigTarget {
                config_path: &config_path,
                recipe_path: Some(&recipe_path),
            };
            run_with_target(&target, action, verbose).await
        }
    }
}

/// Core config logic, operating on a given config path (no recipe persistence).
/// Used by global config commands.
pub async fn run(
    config_path: &Path,
    action: Option<ConfigAction>,
    verbose: u8,
) -> anyhow::Result<()> {
    let target = ConfigTarget {
        config_path,
        recipe_path: None,
    };
    run_with_target(&target, action, verbose).await
}

/// Core config logic with optional recipe dual-write.
async fn run_with_target(
    target: &ConfigTarget<'_>,
    action: Option<ConfigAction>,
    verbose: u8,
) -> anyhow::Result<()> {
    match action {
        Some(ConfigAction::Set { property, value }) => {
            config_set(target, &property, &value)
        }
        Some(ConfigAction::Unset { property }) => config_unset(target, &property),
        Some(ConfigAction::Add { property, value }) => {
            config_add(target, &property, &value)
        }
        Some(ConfigAction::Remove { property, value }) => {
            config_remove(target, &property, &value)
        }
        Some(ConfigAction::List) => config_list(target.config_path),
        None => interactive(target, verbose).await,
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

// --- Pure mutation functions (operate on a JSON object in memory) ---

fn apply_set(obj: &mut serde_json::Map<String, Value>, property: &str, value: &str) -> String {
    let json_value = if let Ok(n) = value.parse::<u64>() {
        Value::Number(n.into())
    } else if let Ok(b) = value.parse::<bool>() {
        Value::Bool(b)
    } else {
        Value::String(value.to_string())
    };
    obj.insert(property.to_string(), json_value);
    format!("Set {property} = {value}")
}

fn apply_unset(obj: &mut serde_json::Map<String, Value>, property: &str) -> String {
    if obj.remove(property).is_some() {
        format!("Removed {property}")
    } else {
        format!("Property {property} not found")
    }
}

fn apply_add(
    obj: &mut serde_json::Map<String, Value>,
    property: &str,
    value: &str,
) -> anyhow::Result<String> {
    match property_type(property) {
        PropertyType::FeatureMap => {
            let features = obj
                .entry(property)
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(map) = features.as_object_mut() {
                map.insert(value.to_string(), serde_json::json!({}));
            }
            Ok(format!("Added feature {value}"))
        }
        PropertyType::Array => {
            let arr = obj
                .entry(property)
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(vec) = arr.as_array_mut() {
                if property == "forwardPorts" {
                    let port: u16 = value
                        .parse()
                        .map_err(|_| anyhow::anyhow!("Invalid port number: {value}"))?;
                    vec.push(Value::Number(port.into()));
                    Ok(format!("Added port {port}"))
                } else {
                    vec.push(Value::String(value.to_string()));
                    Ok(format!("Added {value} to {property}"))
                }
            } else {
                Ok(String::new())
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
            Ok(format!("Set {property}.{key} = {val}"))
        }
        PropertyType::Lifecycle => {
            let (label, cmd) = value.split_once('=').ok_or_else(|| {
                anyhow::anyhow!(
                    "Expected LABEL=COMMAND format for lifecycle add, got: {value}\n\
                     Use 'set' for a simple string command, or 'add' with label=command for the object form"
                )
            })?;

            let entry = obj
                .entry(property)
                .or_insert_with(|| Value::Object(serde_json::Map::new()));

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
            Ok(format!("Added {property}[{label}] = {cmd}"))
        }
        PropertyType::Scalar => {
            anyhow::bail!("Property '{property}' is scalar — use 'set' instead of 'add'")
        }
    }
}

fn apply_remove(
    obj: &mut serde_json::Map<String, Value>,
    property: &str,
    value: &str,
) -> anyhow::Result<String> {
    match property_type(property) {
        PropertyType::FeatureMap => {
            if let Some(features) = obj.get_mut(property).and_then(|v| v.as_object_mut()) {
                if features.remove(value).is_some() {
                    Ok(format!("Removed feature {value}"))
                } else {
                    Ok(format!("Feature {value} not found"))
                }
            } else {
                Ok(format!("No {property} configured"))
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
                    Ok(format!("Removed {value} from {property}"))
                } else {
                    Ok(format!("{value} not found in {property}"))
                }
            } else {
                Ok(format!("No {property} configured"))
            }
        }
        PropertyType::KeyValueMap => {
            if let Some(map) = obj.get_mut(property).and_then(|v| v.as_object_mut()) {
                if map.remove(value).is_some() {
                    Ok(format!("Removed {value} from {property}"))
                } else {
                    Ok(format!("{value} not found in {property}"))
                }
            } else {
                Ok(format!("No {property} configured"))
            }
        }
        PropertyType::Lifecycle => {
            if let Some(map) = obj.get_mut(property).and_then(|v| v.as_object_mut()) {
                if map.remove(value).is_some() {
                    if map.len() == 1 {
                        if let Some((_, single_val)) = map.iter().next() {
                            let simplified = single_val.clone();
                            obj.insert(property.to_string(), simplified);
                        }
                    }
                    Ok(format!("Removed {value} from {property}"))
                } else {
                    Ok(format!("Label '{value}' not found in {property}"))
                }
            } else {
                anyhow::bail!(
                    "{property} is not in object form — use 'unset' to remove it entirely"
                )
            }
        }
        PropertyType::Scalar => {
            anyhow::bail!("Property '{property}' is scalar — use 'unset' instead of 'remove'")
        }
    }
}

// --- Recipe persistence helper ---

/// Persist a mutation to the recipe's customizations field.
fn persist_to_recipe(
    recipe_path: &Path,
    mutate: impl FnOnce(&mut serde_json::Map<String, Value>) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut recipe = Recipe::from_path(recipe_path)?;
    let obj = recipe
        .customizations
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("recipe customizations is not a JSON object"))?;
    mutate(obj)?;
    recipe.write_to(recipe_path)?;
    Ok(())
}

// --- Action handlers (read file, apply mutation, write file, optionally persist to recipe) ---

fn config_set(target: &ConfigTarget<'_>, property: &str, value: &str) -> anyhow::Result<()> {
    let mut json = read_config(target.config_path)?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;
    let msg = apply_set(obj, property, value);
    write_config(target.config_path, &json)?;
    println!("{msg}");

    if let Some(recipe_path) = target.recipe_path {
        persist_to_recipe(recipe_path, |obj| {
            apply_set(obj, property, value);
            Ok(())
        })?;
    }
    Ok(())
}

fn config_unset(target: &ConfigTarget<'_>, property: &str) -> anyhow::Result<()> {
    let mut json = read_config(target.config_path)?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;
    let msg = apply_unset(obj, property);
    write_config(target.config_path, &json)?;
    println!("{msg}");

    if let Some(recipe_path) = target.recipe_path {
        persist_to_recipe(recipe_path, |obj| {
            apply_unset(obj, property);
            Ok(())
        })?;
    }
    Ok(())
}

fn config_add(target: &ConfigTarget<'_>, property: &str, value: &str) -> anyhow::Result<()> {
    let mut json = read_config(target.config_path)?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;
    let msg = apply_add(obj, property, value)?;
    write_config(target.config_path, &json)?;
    println!("{msg}");

    if let Some(recipe_path) = target.recipe_path {
        persist_to_recipe(recipe_path, |obj| {
            apply_add(obj, property, value)?;
            Ok(())
        })?;
    }
    Ok(())
}

fn config_remove(target: &ConfigTarget<'_>, property: &str, value: &str) -> anyhow::Result<()> {
    let mut json = read_config(target.config_path)?;
    let obj = json
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("devcontainer.json is not a JSON object"))?;
    let msg = apply_remove(obj, property, value)?;
    write_config(target.config_path, &json)?;
    println!("{msg}");

    if let Some(recipe_path) = target.recipe_path {
        persist_to_recipe(recipe_path, |obj| {
            apply_remove(obj, property, value)?;
            Ok(())
        })?;
    }
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

async fn interactive(target: &ConfigTarget<'_>, verbose: u8) -> anyhow::Result<()> {
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
        0 => interactive_image(target)?,
        1 => interactive_features(target, verbose).await?,
        2 => interactive_ports(target)?,
        3 => interactive_env(target)?,
        4 => interactive_mounts(target)?,
        5 => interactive_lifecycle(target)?,
        6 => interactive_other(target)?,
        7 => config_list(target.config_path)?,
        _ => unreachable!(),
    }

    Ok(())
}

fn interactive_image(target: &ConfigTarget<'_>) -> anyhow::Result<()> {
    let json = read_config(target.config_path)?;
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
            config_unset(target, "image")?;
        }
    } else {
        config_set(target, "image", &value)?;
    }
    Ok(())
}

async fn interactive_features(target: &ConfigTarget<'_>, _verbose: u8) -> anyhow::Result<()> {
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
            let json = read_config(target.config_path)?;
            let preselected: Vec<String> = json
                .get("features")
                .and_then(|v| v.as_object())
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();

            let selected = prompts::multi_select_features(&features, &preselected)?;

            // Add newly selected features via config_add for proper dual-write
            for feature_ref in &selected {
                config_add(target, "features", feature_ref)?;
            }
            println!("Features updated.");
        }
        1 => {
            // Remove features
            let json = read_config(target.config_path)?;
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
                config_remove(target, "features", &features[idx])?;
            }
        }
        _ => unreachable!(),
    }

    Ok(())
}

fn interactive_ports(target: &ConfigTarget<'_>) -> anyhow::Result<()> {
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
            config_add(target, "forwardPorts", &port)?;
        }
        1 => {
            let json = read_config(target.config_path)?;
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

            config_remove(target, "forwardPorts", &ports[sel])?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn interactive_env(target: &ConfigTarget<'_>) -> anyhow::Result<()> {
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
            config_add(target, env_key, &kv)?;
        }
        1 => {
            let json = read_config(target.config_path)?;
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

            config_remove(target, env_key, &keys[sel])?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn interactive_mounts(target: &ConfigTarget<'_>) -> anyhow::Result<()> {
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
            config_add(target, "mounts", &mount)?;
        }
        1 => {
            let json = read_config(target.config_path)?;
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

            config_remove(target, "mounts", &mounts[sel])?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn interactive_lifecycle(target: &ConfigTarget<'_>) -> anyhow::Result<()> {
    let sel = Select::new()
        .with_prompt("Which lifecycle command?")
        .items(LIFECYCLE_COMMANDS)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    let key = LIFECYCLE_COMMANDS[sel];

    let json = read_config(target.config_path)?;
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
            format!(
                "object with labels: {}",
                labels
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
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
            config_set(target, key, &value)?;
        }
        1 => {
            let label: String = Input::new()
                .with_prompt("Label (e.g. build, test, install)")
                .interact_text()?;
            let cmd: String = Input::new()
                .with_prompt("Command")
                .interact_text()?;
            config_add(target, key, &format!("{label}={cmd}"))?;
        }
        2 => {
            // Show labels to pick from
            let json = read_config(target.config_path)?;
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
                config_remove(target, key, &labels[label_sel])?;
            } else {
                println!("{key} is not in object form — use 'Remove entirely' instead.");
            }
        }
        3 => {
            config_unset(target, key)?;
        }
        _ => unreachable!(),
    }
    Ok(())
}

fn interactive_other(target: &ConfigTarget<'_>) -> anyhow::Result<()> {
    let properties = ["remoteUser", "shutdownAction", "waitFor"];

    let sel = Select::new()
        .with_prompt("Which property?")
        .items(&properties)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("Cancelled"))?;

    let key = properties[sel];

    let json = read_config(target.config_path)?;
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
            config_unset(target, key)?;
        }
    } else {
        config_set(target, key, &value)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::fs;
    use tempfile::TempDir;

    fn setup_config(content: &str) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("devcontainer.json");
        fs::write(&config_path, content).unwrap();
        (dir, config_path)
    }

    fn target_for(path: &Path) -> ConfigTarget<'_> {
        ConfigTarget {
            config_path: path,
            recipe_path: None,
        }
    }

    #[test]
    fn test_config_set_string() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        config_set(&target_for(&path), "image", "alpine").unwrap();
        let json = read_config(&path).unwrap();
        assert_eq!(json["image"], "alpine");
    }

    #[test]
    fn test_config_set_number() {
        let (_dir, path) = setup_config(r#"{}"#);
        config_set(&target_for(&path), "shutdownAction", "none").unwrap();
        let json = read_config(&path).unwrap();
        assert_eq!(json["shutdownAction"], "none");
    }

    #[test]
    fn test_config_set_bool() {
        let (_dir, path) = setup_config(r#"{}"#);
        config_set(&target_for(&path), "updateRemoteUserUID", "true").unwrap();
        let json = read_config(&path).unwrap();
        assert_eq!(json["updateRemoteUserUID"], true);
    }

    #[test]
    fn test_config_unset() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu", "remoteUser": "vscode"}"#);
        config_unset(&target_for(&path), "image").unwrap();
        let json = read_config(&path).unwrap();
        assert!(json.get("image").is_none());
        assert_eq!(json["remoteUser"], "vscode");
    }

    #[test]
    fn test_config_unset_missing() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        // Should not error, just print "not found"
        config_unset(&target_for(&path), "nonexistent").unwrap();
    }

    #[test]
    fn test_config_add_feature() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        config_add(
            &target_for(&path),
            "features",
            "ghcr.io/devcontainers/features/node",
        )
        .unwrap();
        let json = read_config(&path).unwrap();
        assert!(json["features"]["ghcr.io/devcontainers/features/node"].is_object());
    }

    #[test]
    fn test_config_add_port() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        config_add(&target_for(&path), "forwardPorts", "3000").unwrap();
        config_add(&target_for(&path), "forwardPorts", "8080").unwrap();
        let json = read_config(&path).unwrap();
        let ports = json["forwardPorts"].as_array().unwrap();
        assert_eq!(ports.len(), 2);
        assert_eq!(ports[0], 3000);
        assert_eq!(ports[1], 8080);
    }

    #[test]
    fn test_config_add_env() {
        let (_dir, path) = setup_config(r#"{}"#);
        config_add(&target_for(&path), "remoteEnv", "NODE_ENV=development").unwrap();
        let json = read_config(&path).unwrap();
        assert_eq!(json["remoteEnv"]["NODE_ENV"], "development");
    }

    #[test]
    fn test_config_add_mount() {
        let (_dir, path) = setup_config(r#"{}"#);
        config_add(
            &target_for(&path),
            "mounts",
            "source=mydata,target=/data,type=volume",
        )
        .unwrap();
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
        config_remove(
            &target_for(&path),
            "features",
            "ghcr.io/devcontainers/features/node",
        )
        .unwrap();
        let json = read_config(&path).unwrap();
        let features = json["features"].as_object().unwrap();
        assert_eq!(features.len(), 1);
        assert!(features.contains_key("ghcr.io/devcontainers/features/python"));
    }

    #[test]
    fn test_config_remove_port() {
        let (_dir, path) = setup_config(r#"{"forwardPorts": [3000, 8080]}"#);
        config_remove(&target_for(&path), "forwardPorts", "3000").unwrap();
        let json = read_config(&path).unwrap();
        let ports = json["forwardPorts"].as_array().unwrap();
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0], 8080);
    }

    #[test]
    fn test_config_remove_env() {
        let (_dir, path) =
            setup_config(r#"{"remoteEnv": {"NODE_ENV": "development", "DEBUG": "1"}}"#);
        config_remove(&target_for(&path), "remoteEnv", "NODE_ENV").unwrap();
        let json = read_config(&path).unwrap();
        let env = json["remoteEnv"].as_object().unwrap();
        assert_eq!(env.len(), 1);
        assert!(env.contains_key("DEBUG"));
    }

    #[test]
    fn test_config_add_lifecycle_creates_object() {
        let (_dir, path) = setup_config(r#"{}"#);
        config_add(
            &target_for(&path),
            "postCreateCommand",
            "build=npm run build",
        )
        .unwrap();
        let json = read_config(&path).unwrap();
        let obj = json["postCreateCommand"].as_object().unwrap();
        assert_eq!(obj["build"], "npm run build");
    }

    #[test]
    fn test_config_add_lifecycle_merges_into_existing_object() {
        let (_dir, path) =
            setup_config(r#"{"postCreateCommand": {"build": "npm run build"}}"#);
        config_add(&target_for(&path), "postCreateCommand", "test=npm test").unwrap();
        let json = read_config(&path).unwrap();
        let obj = json["postCreateCommand"].as_object().unwrap();
        assert_eq!(obj["build"], "npm run build");
        assert_eq!(obj["test"], "npm test");
    }

    #[test]
    fn test_config_add_lifecycle_converts_string_to_object() {
        let (_dir, path) = setup_config(r#"{"postCreateCommand": "npm install"}"#);
        config_add(
            &target_for(&path),
            "postCreateCommand",
            "build=npm run build",
        )
        .unwrap();
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
        config_add(&target_for(&path), "postCreateCommand", "test=npm test").unwrap();
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
        config_remove(&target_for(&path), "postCreateCommand", "test").unwrap();
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
        config_remove(&target_for(&path), "postCreateCommand", "test").unwrap();
        let json = read_config(&path).unwrap();
        // Should simplify to the remaining value directly
        assert_eq!(json["postCreateCommand"], "npm run build");
    }

    #[test]
    fn test_config_remove_lifecycle_string_errors() {
        let (_dir, path) = setup_config(r#"{"postCreateCommand": "npm install"}"#);
        let result = config_remove(&target_for(&path), "postCreateCommand", "default");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_add_lifecycle_without_equals_errors() {
        let (_dir, path) = setup_config(r#"{}"#);
        let result = config_add(&target_for(&path), "postCreateCommand", "npm install");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_set_lifecycle_as_string() {
        // set still works as simple string
        let (_dir, path) = setup_config(r#"{}"#);
        config_set(&target_for(&path), "postCreateCommand", "npm install").unwrap();
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
        let result = config_add(&target_for(&path), "image", "alpine");
        assert!(result.is_err());
    }

    #[test]
    fn test_config_remove_scalar_errors() {
        let (_dir, path) = setup_config(r#"{"image": "ubuntu"}"#);
        let result = config_remove(&target_for(&path), "image", "ubuntu");
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

    // --- Recipe dual-write tests ---

    fn setup_recipe(config_content: &str) -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let config_path = dir.path().join("devcontainer.json");
        let recipe_path = dir.path().join("recipe.json");
        fs::write(&config_path, config_content).unwrap();
        let recipe = Recipe {
            global_template: "test".to_string(),
            features: Vec::new(),
            options: HashMap::new(),
            root_folder: "/tmp/proj".to_string(),
            customizations: Value::Object(serde_json::Map::new()),
        };
        recipe.write_to(&recipe_path).unwrap();
        (dir, config_path, recipe_path)
    }

    #[test]
    fn test_recipe_dual_write_set() {
        let (_dir, config_path, recipe_path) = setup_recipe(r#"{"image": "ubuntu"}"#);
        let target = ConfigTarget {
            config_path: &config_path,
            recipe_path: Some(&recipe_path),
        };
        config_set(&target, "remoteUser", "vscode").unwrap();

        // Verify composed config
        let json = read_config(&config_path).unwrap();
        assert_eq!(json["remoteUser"], "vscode");

        // Verify recipe customizations
        let recipe = Recipe::from_path(&recipe_path).unwrap();
        assert_eq!(recipe.customizations["remoteUser"], "vscode");
    }

    #[test]
    fn test_recipe_dual_write_unset() {
        let (_dir, config_path, recipe_path) = setup_recipe(r#"{"remoteUser": "vscode"}"#);
        let target = ConfigTarget {
            config_path: &config_path,
            recipe_path: Some(&recipe_path),
        };

        // First set via recipe, then unset
        config_set(&target, "remoteUser", "developer").unwrap();
        config_unset(&target, "remoteUser").unwrap();

        let recipe = Recipe::from_path(&recipe_path).unwrap();
        assert!(recipe.customizations.get("remoteUser").is_none());
    }

    #[test]
    fn test_recipe_dual_write_add_port() {
        let (_dir, config_path, recipe_path) = setup_recipe(r#"{"forwardPorts": [3000]}"#);
        let target = ConfigTarget {
            config_path: &config_path,
            recipe_path: Some(&recipe_path),
        };
        config_add(&target, "forwardPorts", "9090").unwrap();

        // Verify composed config has both
        let json = read_config(&config_path).unwrap();
        let ports = json["forwardPorts"].as_array().unwrap();
        assert_eq!(ports.len(), 2);

        // Verify recipe customizations has only the addition
        let recipe = Recipe::from_path(&recipe_path).unwrap();
        let recipe_ports = recipe.customizations["forwardPorts"].as_array().unwrap();
        assert_eq!(recipe_ports.len(), 1);
        assert_eq!(recipe_ports[0], 9090);
    }

    #[test]
    fn test_recipe_dual_write_add_env() {
        let (_dir, config_path, recipe_path) = setup_recipe(r#"{}"#);
        let target = ConfigTarget {
            config_path: &config_path,
            recipe_path: Some(&recipe_path),
        };
        config_add(&target, "remoteEnv", "MY_VAR=hello").unwrap();

        let recipe = Recipe::from_path(&recipe_path).unwrap();
        assert_eq!(recipe.customizations["remoteEnv"]["MY_VAR"], "hello");
    }

    #[test]
    fn test_recipe_dual_write_remove_from_customizations() {
        let (_dir, config_path, recipe_path) = setup_recipe(r#"{"forwardPorts": [3000]}"#);
        let target = ConfigTarget {
            config_path: &config_path,
            recipe_path: Some(&recipe_path),
        };
        // Add then remove
        config_add(&target, "forwardPorts", "9090").unwrap();
        config_remove(&target, "forwardPorts", "9090").unwrap();

        let recipe = Recipe::from_path(&recipe_path).unwrap();
        let recipe_ports = recipe.customizations["forwardPorts"].as_array().unwrap();
        assert!(recipe_ports.is_empty());
    }
}
