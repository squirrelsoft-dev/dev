use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::error::DevError;

/// Apply a devcontainer template: copy files from `artifact_path` to `dest`,
/// replacing `${templateOption:optionId}` placeholders with the provided options.
pub fn apply_template(
    artifact_path: &Path,
    options: &HashMap<String, String>,
    dest: &Path,
) -> Result<(), DevError> {
    copy_and_substitute(artifact_path, dest, options)
}

fn copy_and_substitute(
    src: &Path,
    dest: &Path,
    options: &HashMap<String, String>,
) -> Result<(), DevError> {
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name_str = file_name.to_string_lossy();

        // Skip the template metadata file.
        if file_name_str == "devcontainer-template.json" {
            continue;
        }

        let src_path = entry.path();
        let dest_path = dest.join(&file_name);

        if src_path.is_dir() {
            fs::create_dir_all(&dest_path)?;
            copy_and_substitute(&src_path, &dest_path, options)?;
        } else {
            // Try to read as text for substitution; copy binary files as-is.
            match fs::read_to_string(&src_path) {
                Ok(content) => {
                    let substituted = substitute_options(&content, options);
                    fs::write(&dest_path, substituted)?;
                }
                Err(_) => {
                    fs::copy(&src_path, &dest_path)?;
                }
            }
        }
    }
    Ok(())
}

/// Replace all `${templateOption:optionId}` placeholders in the text.
pub(crate) fn substitute_options(text: &str, options: &HashMap<String, String>) -> String {
    let mut result = text.to_string();
    for (key, value) in options {
        let placeholder = format!("${{templateOption:{key}}}");
        result = result.replace(&placeholder, value);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_substitute_options() {
        let mut opts = HashMap::new();
        opts.insert("imageVariant".to_string(), "3.11".to_string());
        opts.insert("nodeVersion".to_string(), "18".to_string());

        let input = "FROM python:${templateOption:imageVariant}\nRUN nvm install ${templateOption:nodeVersion}";
        let result = substitute_options(input, &opts);
        assert_eq!(result, "FROM python:3.11\nRUN nvm install 18");
    }

    #[test]
    fn test_substitute_no_match() {
        let opts = HashMap::new();
        let input = "no placeholders here";
        assert_eq!(substitute_options(input, &opts), "no placeholders here");
    }
}
