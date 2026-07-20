use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::collection::{
    Collection, TemplateMetadata, TemplateTier, fetch_all_features, fetch_collection_index,
    fetch_templates, template_collections, template_tier,
};
use crate::devcontainer::apply_template;
use crate::devcontainer::recipe::Recipe;
use crate::oci::download_artifact;
use crate::tui::{picker, prompts};
use crate::util::paths::{DevHome, devcontainers_dir, global_dir};
use crate::util::workspace_folder_name;

pub async fn run(
    workspace: &Path,
    template: Option<&str>,
    options: &[String],
    verbose: u8,
) -> anyhow::Result<()> {
    eprintln!("Fetching template catalog...");
    let collections = fetch_collection_index(false).await?;

    // Check for existing global templates
    let global_names = list_global_template_names();
    let has_globals = !global_names.is_empty();

    // If --template was passed, use the old direct flow (skip source picker)
    let (oci_ref, selected) = if let Some(id) = template {
        let all_templates = fetch_all_templates(&collections, verbose).await?;
        all_templates
            .into_iter()
            .find(|(_, t)| t.id == id)
            .ok_or_else(|| anyhow::anyhow!("Template '{id}' not found"))?
    } else {
        // Interactive source picker
        let source = picker::pick_source(has_globals)?;

        match source {
            picker::TemplateSource::ExistingGlobal => {
                let name = picker::pick_global_template(&global_names)?;
                return apply_global_template(workspace, &name, options, verbose).await;
            }
            _ => {
                let templates = fetch_templates_for_source(source, &collections, verbose).await?;
                if templates.is_empty() {
                    anyhow::bail!("No templates available in the selected category.");
                }
                let (oci_ref, t) = picker::pick_template(&templates)?;
                (oci_ref, t.clone())
            }
        }
    };

    // Feature multi-select
    let selected_features = select_features(&collections, &[], verbose).await?;

    // Scope picker
    let scope = picker::pick_scope()?;

    // Download the template artifact
    let version = if selected.version.is_empty() {
        "latest"
    } else {
        &selected.version
    };
    let artifact_ref = format!("{}/{}", oci_ref, selected.id);
    eprintln!("Downloading template '{}'...", selected.id);
    let artifact_path = download_artifact(&artifact_ref, version).await?;

    // Parse options from CLI args or prompt interactively
    let mut opts = parse_option_args(options);
    if !selected.options.is_empty() {
        let missing: Vec<_> = selected
            .options
            .iter()
            .filter(|o| !opts.contains_key(&o.id))
            .cloned()
            .collect();
        if !missing.is_empty() {
            let prompted = prompts::prompt_options(&missing)?;
            opts.extend(prompted);
        }
    }

    // Save the template as a global template so the recipe can reference the
    // durable template config instead of baking it into this project.
    let global_name = ensure_global_template(&selected.id, &artifact_path)?;

    // Write a recipe to the chosen scope.
    match scope {
        picker::Scope::Workspace => {
            let devcontainer_dir = workspace.join(".devcontainer");
            ensure_recipe_can_own_workspace(workspace, &devcontainer_dir)?;
            write_recipe(&devcontainer_dir, &global_name, selected_features, opts)?;
            println!(
                "Recipe for template '{}' written to {}",
                selected.id,
                devcontainer_dir.join("recipe.json").display()
            );
        }
        picker::Scope::User => {
            let folder_name = workspace_folder_name(workspace);
            let user_dest = devcontainers_dir().join(&folder_name);
            let devcontainer_dir = user_dest.join(".devcontainer");
            write_recipe(&devcontainer_dir, &global_name, selected_features, opts)?;
            println!(
                "Recipe for template '{}' written to {}",
                selected.id,
                devcontainer_dir.join("recipe.json").display()
            );
            print_user_scope_hint();
        }
    };

    Ok(())
}

/// A user-scoped recipe has no `devcontainer.json` for VS Code's remote-containers
/// extension to open, so point the user at the flow that does work.
fn print_user_scope_hint() {
    println!("Run `dev up` to start the container, then attach to it from your editor.");
}

/// Apply an existing global template to the workspace.
async fn apply_global_template(
    workspace: &Path,
    global_name: &str,
    options: &[String],
    verbose: u8,
) -> anyhow::Result<()> {
    let global_path = global_dir().join(global_name);
    if !global_path.is_dir() {
        anyhow::bail!("Global template '{global_name}' not found");
    }

    // Read existing features from the global template's devcontainer.json
    let existing_features = read_existing_features(&global_path);

    // Feature multi-select with existing features pre-checked
    eprintln!("Fetching feature catalog...");
    let collections = fetch_collection_index(false).await?;
    let selected_features = select_features(&collections, &existing_features, verbose).await?;

    let scope = picker::pick_scope()?;
    let opts = parse_option_args(options);

    match scope {
        picker::Scope::Workspace => {
            let devcontainer_dir = workspace.join(".devcontainer");
            ensure_recipe_can_own_workspace(workspace, &devcontainer_dir)?;
            write_recipe(&devcontainer_dir, global_name, selected_features, opts)?;
            println!(
                "Recipe for global template '{global_name}' written to {}",
                devcontainer_dir.join("recipe.json").display()
            );
        }
        picker::Scope::User => {
            let folder_name = workspace_folder_name(workspace);
            let devcontainer_dir = devcontainers_dir().join(&folder_name).join(".devcontainer");
            write_recipe(&devcontainer_dir, global_name, selected_features, opts)?;
            println!(
                "Recipe for global template '{global_name}' written to {}",
                devcontainer_dir.join("recipe.json").display()
            );
            print_user_scope_hint();
        }
    }

    Ok(())
}

/// Refuse to create a recipe next to a `devcontainer.json`.
///
/// `find_config_source` rejects a directory holding both, so writing one anyway
/// would leave every later `dev up`/`build`/`config` failing until the user
/// deleted a file by hand.
fn ensure_recipe_can_own_workspace(workspace: &Path, devcontainer_dir: &Path) -> anyhow::Result<()> {
    for existing in [
        devcontainer_dir.join("devcontainer.json"),
        workspace.join(".devcontainer.json"),
    ] {
        if existing.is_file() {
            anyhow::bail!(
                "{} already exists.\n\
                 `dev new` writes a .devcontainer/recipe.json, and a workspace cannot have both. \
                 Move or delete the devcontainer.json first, then re-run `dev new`.",
                existing.display()
            );
        }
    }
    Ok(())
}

fn write_recipe(
    devcontainer_dir: &Path,
    global_template: &str,
    features: Vec<String>,
    options: HashMap<String, String>,
) -> anyhow::Result<()> {
    write_recipe_in(
        &DevHome::current(),
        devcontainer_dir,
        global_template,
        features,
        options,
    )
}

fn write_recipe_in(
    dev_home: &DevHome,
    devcontainer_dir: &Path,
    global_template: &str,
    features: Vec<String>,
    options: HashMap<String, String>,
) -> anyhow::Result<()> {
    let recipe_path = devcontainer_dir.join("recipe.json");
    let previous = Recipe::from_path(&recipe_path).ok();
    let recipe = Recipe {
        global_template: global_template.to_string(),
        features,
        options,
        customizations: serde_json::Value::Object(serde_json::Map::new()),
    };
    // Auxiliary files are planned (and rejected if locally edited) before the
    // recipe is written, so a refusal leaves the project exactly as it was.
    crate::devcontainer::compose::prepare_recipe_directory_in(
        dev_home,
        &recipe,
        devcontainer_dir,
        crate::devcontainer::compose::AuxPolicy::Refresh {
            previous: previous.as_ref(),
        },
    )?;
    recipe.write_to(&recipe_path)?;
    Ok(())
}

/// Fetch templates only from the collections matching the chosen source category.
async fn fetch_templates_for_source(
    source: picker::TemplateSource,
    collections: &[Collection],
    verbose: u8,
) -> anyhow::Result<Vec<(String, TemplateMetadata)>> {
    let all_template_cols = template_collections(collections);
    let target_tier = match source {
        picker::TemplateSource::Official => TemplateTier::Official,
        picker::TemplateSource::Microsoft => TemplateTier::Microsoft,
        picker::TemplateSource::Community => TemplateTier::Community,
        picker::TemplateSource::ExistingGlobal => {
            unreachable!("handled before this function is called");
        }
    };
    let filtered: Vec<&&Collection> = all_template_cols
        .iter()
        .filter(|c| template_tier(c) == target_tier)
        .collect();

    let mut templates = Vec::new();
    for c in &filtered {
        match fetch_templates(c, false).await {
            Ok(ts) => {
                for t in ts {
                    templates.push((c.oci_ref.clone(), t));
                }
            }
            Err(e) => {
                if verbose > 0 {
                    eprintln!("Warning: failed to fetch from '{}': {e}", c.name);
                }
            }
        }
    }
    Ok(templates)
}

/// Fetch all templates from template collections.
async fn fetch_all_templates(
    collections: &[Collection],
    verbose: u8,
) -> anyhow::Result<Vec<(String, TemplateMetadata)>> {
    let template_cols = template_collections(collections);
    let fetches: Vec<_> = template_cols
        .iter()
        .map(|c| fetch_templates(c, false))
        .collect();
    let results = futures_util::future::join_all(fetches).await;

    let mut all = Vec::new();
    for (collection, result) in template_cols.iter().zip(results) {
        match result {
            Ok(templates) => {
                for t in templates {
                    all.push((collection.oci_ref.clone(), t));
                }
            }
            Err(e) => {
                if verbose > 0 {
                    eprintln!("Warning: failed to fetch from '{}': {e}", collection.name);
                }
            }
        }
    }
    Ok(all)
}

/// Fetch features from all feature collections and present multi-select.
/// `preselected` contains feature refs already in the template (pre-checked in the list).
async fn select_features(
    collections: &[Collection],
    preselected: &[String],
    _verbose: u8,
) -> anyhow::Result<Vec<String>> {
    let features = fetch_all_features(collections, false).await;
    if features.is_empty() {
        return Ok(Vec::new());
    }
    prompts::multi_select_features(&features, preselected)
}

/// Get the list of global template names from ~/.dev/global/.
fn list_global_template_names() -> Vec<String> {
    let dir = global_dir();
    if !dir.is_dir() {
        return Vec::new();
    }
    let mut names = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                names.push(entry.file_name().to_string_lossy().to_string());
            }
        }
    }
    names.sort();
    names
}

/// Read feature refs from an existing devcontainer.json's "features" map.
fn read_existing_features(template_dir: &Path) -> Vec<String> {
    let config_path = template_dir.join(".devcontainer/devcontainer.json");
    let raw = match fs::read_to_string(&config_path) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let json: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    json.get("features")
        .and_then(|f| f.as_object())
        .map(|obj| obj.keys().cloned().collect())
        .unwrap_or_default()
}

/// Ensure a template is saved as a global template at `~/.dev/global/<name>/`.
/// If it already exists, this is a no-op. Returns the global template name.
fn ensure_global_template(template_id: &str, artifact_path: &Path) -> anyhow::Result<String> {
    let name = template_id.to_string();
    let global_path = global_dir().join(&name);
    if global_path
        .join(".devcontainer/devcontainer.json")
        .is_file()
    {
        return Ok(name);
    }
    fs::create_dir_all(&global_path)?;
    // Copy the template without substituting options — keep placeholders for reuse
    apply_template(artifact_path, &HashMap::new(), &global_path)?;
    eprintln!(
        "Saved as global template '{name}' at {}",
        global_path.display()
    );
    Ok(name)
}

fn parse_option_args(args: &[String]) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for arg in args {
        if let Some((key, val)) = arg.split_once('=') {
            map.insert(key.to_string(), val.to_string());
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn write_recipe_creates_recipe_and_auxiliary_files_without_composed_config() {
        let home = TempDir::new().unwrap();
        let dev_home = DevHome::at(home.path());
        let global_dir = home.path().join("global/rust/.devcontainer");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("devcontainer.json"),
            r#"{"image":"rust:latest"}"#,
        )
        .unwrap();
        fs::write(
            global_dir.join("Dockerfile"),
            "FROM ${templateOption:base}\n",
        )
        .unwrap();

        let workspace = TempDir::new().unwrap();
        let devcontainer_dir = workspace.path().join(".devcontainer");
        write_recipe_in(
            &dev_home,
            &devcontainer_dir,
            "rust",
            vec!["ghcr.io/features/node:1".to_string()],
            HashMap::from([("base".to_string(), "rust:latest".to_string())]),
        )
        .unwrap();

        let recipe = Recipe::from_path(&devcontainer_dir.join("recipe.json")).unwrap();
        assert_eq!(recipe.global_template, "rust");
        assert_eq!(recipe.features, vec!["ghcr.io/features/node:1"]);
        assert_eq!(recipe.options["base"], "rust:latest");
        assert_eq!(
            fs::read_to_string(devcontainer_dir.join("Dockerfile")).unwrap(),
            "FROM rust:latest\n"
        );
        assert!(
            !devcontainer_dir.join("devcontainer.json").exists(),
            "dev new must not persist a composed devcontainer.json for recipe projects"
        );
    }

    #[test]
    fn a_written_recipe_carries_no_absolute_host_path() {
        let home = TempDir::new().unwrap();
        let global_dir = home.path().join("global/rust/.devcontainer");
        fs::create_dir_all(&global_dir).unwrap();
        fs::write(
            global_dir.join("devcontainer.json"),
            r#"{"image":"rust:latest"}"#,
        )
        .unwrap();

        let workspace = TempDir::new().unwrap();
        let devcontainer_dir = workspace.path().join(".devcontainer");
        write_recipe_in(
            &DevHome::at(home.path()),
            &devcontainer_dir,
            "rust",
            Vec::new(),
            HashMap::new(),
        )
        .unwrap();

        let raw = fs::read_to_string(devcontainer_dir.join("recipe.json")).unwrap();
        assert!(
            !raw.contains(workspace.path().to_str().unwrap()),
            "a committed recipe must not pin the creating machine's path: {raw}"
        );
        assert!(
            !raw.contains("rootFolder"),
            "the unused rootFolder field must not be serialized: {raw}"
        );
    }

    #[test]
    fn a_workspace_devcontainer_json_blocks_recipe_creation() {
        let workspace = TempDir::new().unwrap();
        let devcontainer_dir = workspace.path().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::write(
            devcontainer_dir.join("devcontainer.json"),
            r#"{"image":"ubuntu"}"#,
        )
        .unwrap();

        let err = ensure_recipe_can_own_workspace(workspace.path(), &devcontainer_dir).unwrap_err();

        assert!(
            err.to_string().contains("devcontainer.json"),
            "the refusal should name the conflicting file: {err}"
        );
    }

    #[test]
    fn a_root_level_devcontainer_json_blocks_recipe_creation() {
        let workspace = TempDir::new().unwrap();
        fs::write(
            workspace.path().join(".devcontainer.json"),
            r#"{"image":"ubuntu"}"#,
        )
        .unwrap();

        assert!(
            ensure_recipe_can_own_workspace(
                workspace.path(),
                &workspace.path().join(".devcontainer")
            )
            .is_err()
        );
    }

    #[test]
    fn a_clean_workspace_accepts_a_recipe() {
        let workspace = TempDir::new().unwrap();

        assert!(
            ensure_recipe_can_own_workspace(
                workspace.path(),
                &workspace.path().join(".devcontainer")
            )
            .is_ok()
        );
    }
}
