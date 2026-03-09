use dialoguer::{FuzzySelect, Select};

use crate::collection::TemplateMetadata;
use crate::tui::{term_dimensions, truncate_to_width};

/// Where the user wants to source the template from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateSource {
    ExistingGlobal,
    Official,
    Microsoft,
    Community,
}

/// Where the devcontainer config should be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Workspace,
    User,
}

/// Present a picker for the template source category.
pub fn pick_source(has_globals: bool) -> anyhow::Result<TemplateSource> {
    let mut items = Vec::new();
    let mut sources = Vec::new();

    if has_globals {
        items.push("Existing global template");
        sources.push(TemplateSource::ExistingGlobal);
    }

    items.push("Official templates (ghcr.io/devcontainers)");
    sources.push(TemplateSource::Official);

    items.push("Microsoft templates (ghcr.io/microsoft)");
    sources.push(TemplateSource::Microsoft);

    items.push("Community templates");
    sources.push(TemplateSource::Community);

    let dims = term_dimensions();
    let selection = Select::new()
        .with_prompt("Where to get the template")
        .items(&items)
        .default(0)
        .max_length(dims.max_length)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("No source selected"))?;

    Ok(sources[selection])
}

/// Present a picker to select one of the user's global templates by name.
pub fn pick_global_template(names: &[String]) -> anyhow::Result<String> {
    let dims = term_dimensions();
    let display: Vec<String> = names
        .iter()
        .map(|n| truncate_to_width(n, dims.max_width))
        .collect();
    let selection = Select::new()
        .with_prompt("Select global template")
        .items(&display)
        .default(0)
        .max_length(dims.max_length)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("No template selected"))?;

    Ok(names[selection].clone())
}

/// Present a picker to choose where the devcontainer config is created.
pub fn pick_scope() -> anyhow::Result<Scope> {
    let items = [
        "Workspace (.devcontainer/ in project)",
        "User scope (~/.dev/devcontainers/)",
    ];

    let selection = Select::new()
        .with_prompt("Where should the devcontainer be created?")
        .items(&items)
        .default(0)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("No scope selected"))?;

    Ok(if selection == 0 {
        Scope::Workspace
    } else {
        Scope::User
    })
}

/// Present an interactive fuzzy-select picker for templates.
/// Returns the OCI reference prefix and the selected template.
pub fn pick_template<'a>(
    templates: &'a [(String, TemplateMetadata)],
) -> anyhow::Result<(String, &'a TemplateMetadata)> {
    let dims = term_dimensions();
    let display_items: Vec<String> = templates
        .iter()
        .map(|(_, t)| {
            let raw = if t.description.is_empty() {
                t.id.clone()
            } else {
                format!("{} - {}", t.id, t.description)
            };
            truncate_to_width(&raw, dims.max_width)
        })
        .collect();

    let selection = FuzzySelect::new()
        .with_prompt("Select a template")
        .items(&display_items)
        .default(0)
        .max_length(dims.max_length)
        .interact_opt()?
        .ok_or_else(|| anyhow::anyhow!("No template selected"))?;

    let (ref oci_ref, ref meta) = templates[selection];
    Ok((oci_ref.clone(), meta))
}
