pub mod config;
pub mod storage;
pub mod indexer;
pub mod mcp;
pub mod ui;

use std::sync::Arc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("=== Dehydrator4Win AI Context OS starting ===");

    // 1. 命令行参数解析
    let args: Vec<String> = std::env::args().collect();
    let headless = args.contains(&"--headless".to_string());

    // 2. 查找或就地创建 config 目录并加载所有的 profile
    let config_dir = std::env::current_dir()?.join("config");
    std::fs::create_dir_all(&config_dir)?;

    let mut profiles = Vec::new();
    for entry in std::fs::read_dir(&config_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if ext == "yaml" || ext == "yml" {
                    match config::Profile::load_from_file(&path) {
                        Ok(prof) => {
                            if !profiles.iter().any(|p: &config::Profile| p.name == prof.name) {
                                profiles.push(prof);
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to load config file {:?}: {}", path, e);
                        }
                    }
                }
            }
        }
    }

    if profiles.is_empty() {
        let profile_path = config_dir.join("default_profile.yaml");
        eprintln!("No profile found. Generating default development profile at: {}", profile_path.display());
        let default_prof = config::Profile {
            name: "default-dev".to_string(),
            description: "Auto-generated default workspace profile".to_string(),
            workspaces: vec![config::WorkspaceFolder {
                path: std::env::current_dir()?,
                tags: vec!["current_project".to_string()],
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
                "*.so".to_string(),
                "*.dll".to_string(),
                "*.class".to_string(),
                "*.apk".to_string(),
                "*.jar".to_string(),
            ],
            max_file_read_lines: 100, // 大于 100 行自动进行骨架脱水
        };
        default_prof.save_to_file(&profile_path)?;
        profiles.push(default_prof);
    }

    // 3. 初始化本地 SQLite 代码图谱符号库
    let db_path = std::env::current_dir()?.join(".dehydrator4win.db");
    eprintln!("Initializing local SQLite CodeGraph at: {}", db_path.display());
    let db = Arc::new(storage::CodeGraph::open(db_path)?);

    // 4. 创建共享配置指针和消息通道
    let first_profile = profiles.first().cloned();
    let active_profile = Arc::new(std::sync::RwLock::new(first_profile));

    // 创建 Tokio runtime
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // 5. 启动后台异步 MCP TCP 服务端 (127.0.0.1:3001)
    let db_clone = db.clone();
    let profile_ref_clone = active_profile.clone();
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let ui_tx_clone = ui_tx.clone();

    rt.spawn(async move {
        if let Err(e) = mcp::run_mcp_server("127.0.0.1:3001", db_clone, profile_ref_clone, ui_tx_clone).await {
            eprintln!("[FATAL ERROR] MCP TCP Server crashed: {}", e);
        }
    });

    // 6. 运行 UI 还是后台常驻
    if headless {
        eprintln!("Running in --headless daemon mode. Listening on 127.0.0.1:3001 and stdin/stdout...");
        let _ = ui_tx.send(mcp::McpEvent::Log("[SYSTEM] Running in headless stdio mode.".to_string()));
        
        // Spawn a task to drain ui_rx and write to stderr in headless mode
        let mut rx = ui_rx;
        rt.spawn(async move {
            while let Some(event) = rx.recv().await {
                match event {
                    mcp::McpEvent::Log(msg) => {
                        eprintln!("[Headless Log] {}", msg);
                    }
                    mcp::McpEvent::TokenSaved { path, saved_tokens } => {
                        eprintln!("[Headless Log] Token saved for {}: {}", path, saved_tokens);
                    }
                }
            }
        });

        rt.block_on(async {
            if let Err(e) = mcp::run_mcp_stdio_server(db, active_profile, ui_tx).await {
                eprintln!("[FATAL ERROR] MCP Stdio Server crashed: {}", e);
            }
        });
    } else {
        eprintln!("Launching GUI dashboard...");
        // 移交控制权给 Iced (阻塞主线程)
        if let Err(e) = ui::run_ui(db, profiles, active_profile, ui_rx) {
            eprintln!("[FATAL ERROR] UI Loop crashed: {:?}", e);
            return Err(Box::new(e) as Box<dyn std::error::Error>);
        }
    }

    Ok(())
}
