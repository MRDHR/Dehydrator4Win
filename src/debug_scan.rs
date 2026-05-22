use std::sync::Arc;
use crate::config::Profile;
use crate::storage::CodeGraph;
use crate::indexer::process_file;
use ignore::WalkBuilder;

pub fn run_debug_scan() -> Result<(), Box<dyn std::error::Error>> {
    println!("Starting debug scan...");
    let db = Arc::new(CodeGraph::open("debug.db")?);
    let current_dir = std::env::current_dir()?;
    println!("Current dir: {:?}", current_dir);

    let profile = Profile {
        name: "debug-dev".to_string(),
        description: "Debug scan".to_string(),
        workspaces: vec![crate::config::WorkspaceFolder {
            path: current_dir.clone(),
            tags: vec![],
        }],
        exclude: vec![
            "target/".to_string(),
            ".git/".to_string(),
            "build/".to_string(),
            ".gradle/".to_string(),
            "node_modules/".to_string(),
            "bin/".to_string(),
            "obj/".to_string(),
            ".idea/".to_string(),
            ".vscode/".to_string(),
            "*.db".to_string(),
            "*.exe".to_string(),
        ],
        max_file_read_lines: 100,
    };

    for workspace in &profile.workspaces {
        if !workspace.path.exists() {
            continue;
        }

        println!("Walking workspace path: {:?}", workspace.path);
        let mut builder = WalkBuilder::new(&workspace.path);
        
        let mut override_builder = ignore::overrides::OverrideBuilder::new(&workspace.path);
        for pattern in &profile.exclude {
            let glob_pattern = format!("!{}", pattern);
            if let Err(err) = override_builder.add(&glob_pattern) {
                println!("Error adding pattern {}: {}", pattern, err);
            }
        }

        if let Ok(overrides) = override_builder.build() {
            builder.overrides(overrides);
        }

        let walker = builder.build();
        for result in walker {
            match result {
                Ok(entry) => {
                    let path = entry.path();
                    if path.is_file() {
                        if let Some(path_str) = path.to_str() {
                            println!("Processing file: {}", path_str);
                            if let Err(err) = process_file(&db, &profile.name, path, path_str, 100) {
                                println!("  Error: {}", err);
                            }
                            println!("  Finished: {}", path_str);
                        }
                    }
                }
                Err(err) => {
                    println!("Walk error: {}", err);
                }
            }
        }
    }

    println!("Debug scan completed successfully!");
    Ok(())
}
