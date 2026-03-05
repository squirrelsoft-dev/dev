use crate::collection::{
    fetch_collection_index, fetch_features, fetch_templates,
};

pub async fn run(
    kind: &str,
    query: Option<&str>,
    json: bool,
    refresh: bool,
    verbose: u8,
) -> anyhow::Result<()> {
    let collections = fetch_collection_index(refresh).await?;

    match kind {
        "templates" => {
            let fetches: Vec<_> = collections
                .iter()
                .map(|c| fetch_templates(c, refresh))
                .collect();
            let results = futures_util::future::join_all(fetches).await;

            let mut all = Vec::new();
            for (collection, result) in collections.iter().zip(results) {
                match result {
                    Ok(templates) if !templates.is_empty() => {
                        for t in templates {
                            all.push((&collection.name, t));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        if verbose > 0 {
                            eprintln!("Warning: failed to fetch templates from '{}': {e}", collection.name);
                        }
                    }
                }
            }

            // Filter by query
            if let Some(q) = query {
                let q_lower = q.to_lowercase();
                all.retain(|(_, t)| {
                    t.id.to_lowercase().contains(&q_lower)
                        || t.name.to_lowercase().contains(&q_lower)
                        || t.description.to_lowercase().contains(&q_lower)
                });
            }

            if json {
                let items: Vec<serde_json::Value> = all
                    .iter()
                    .map(|(coll, t)| {
                        serde_json::json!({
                            "collection": coll,
                            "id": t.id,
                            "name": t.name,
                            "description": t.description,
                            "version": t.version,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if all.is_empty() {
                println!("No templates found.");
            } else {
                println!("{:<40} {:<20} {}", "ID", "NAME", "DESCRIPTION");
                for (_, t) in &all {
                    println!("{:<40} {:<20} {}", t.id, t.name, t.description);
                }
            }
        }
        "features" => {
            let fetches: Vec<_> = collections
                .iter()
                .map(|c| fetch_features(c, refresh))
                .collect();
            let results = futures_util::future::join_all(fetches).await;

            let mut all = Vec::new();
            for (collection, result) in collections.iter().zip(results) {
                match result {
                    Ok(features) if !features.is_empty() => {
                        for f in features {
                            all.push((&collection.name, f));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        if verbose > 0 {
                            eprintln!("Warning: failed to fetch features from '{}': {e}", collection.name);
                        }
                    }
                }
            }

            if let Some(q) = query {
                let q_lower = q.to_lowercase();
                all.retain(|(_, f)| {
                    f.id.to_lowercase().contains(&q_lower)
                        || f.name.to_lowercase().contains(&q_lower)
                        || f.description.to_lowercase().contains(&q_lower)
                });
            }

            if json {
                let items: Vec<serde_json::Value> = all
                    .iter()
                    .map(|(coll, f)| {
                        serde_json::json!({
                            "collection": coll,
                            "id": f.id,
                            "name": f.name,
                            "description": f.description,
                            "version": f.version,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if all.is_empty() {
                println!("No features found.");
            } else {
                println!("{:<40} {:<20} {}", "ID", "NAME", "DESCRIPTION");
                for (_, f) in &all {
                    println!("{:<40} {:<20} {}", f.id, f.name, f.description);
                }
            }
        }
        other => {
            anyhow::bail!("Unknown kind: '{other}'. Use 'templates' or 'features'.");
        }
    }

    Ok(())
}
