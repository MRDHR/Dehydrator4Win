use crate::config::Profile;
use crate::storage::CodeGraph;
use notify::{RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// 检查路径是否被排除过滤规则排除
fn is_path_excluded(path: &Path, exclude_patterns: &[String]) -> bool {
    let path_str = path.to_string_lossy().replace('\\', "/");
    for pattern in exclude_patterns {
        let pattern_clean = pattern.trim_end_matches('/');
        if pattern_clean.is_empty() {
            continue;
        }
        if pattern.ends_with('/') {
            let parts: Vec<&str> = path_str.split('/').collect();
            if parts.iter().any(|&p| p == pattern_clean) {
                return true;
            }
        } else if pattern.starts_with('*') {
            let ext = pattern.trim_start_matches('*');
            if path_str.ends_with(ext) {
                return true;
            }
        } else {
            if path_str.contains(pattern_clean) {
                return true;
            }
        }
    }
    false
}

/// 启动 Watchdog 守护线程，实时增量同步文件变更
pub fn start_watcher(
    db: Arc<CodeGraph>,
    active_profile: Arc<RwLock<Option<Profile>>>,
) {
    std::thread::spawn(move || {
        let mut current_profile_name: Option<String> = None;
        let mut current_workspaces: Vec<PathBuf> = Vec::new();
        let mut watcher: Option<RecommendedWatcher> = None;

        // 文件变动事件通道
        let (tx, rx) = std::sync::mpsc::channel();

        // 防抖哈希表：存放文件路径 -> (防抖触发时间戳, 事件类型)
        let mut debounce_map: std::collections::HashMap<PathBuf, (Instant, notify::EventKind)> =
            std::collections::HashMap::new();

        let mut last_profile_check = Instant::now();

        eprintln!("[Watchdog] Background file monitor thread started.");

        loop {
            let now = Instant::now();

            // 1. 每隔1秒检测一次活动 Profile 是否发生变化
            if now.duration_since(last_profile_check) >= Duration::from_secs(1) || watcher.is_none() {
                last_profile_check = now;

                let (profile_name, workspaces, _exclude_patterns) = {
                    let guard = active_profile.read().unwrap();
                    if let Some(prof) = guard.as_ref() {
                        let ws = prof.workspaces.iter().map(|w| w.path.clone()).collect::<Vec<_>>();
                        (Some(prof.name.clone()), ws, prof.exclude.clone())
                    } else {
                        (None, Vec::new(), Vec::new())
                    }
                };

                if profile_name != current_profile_name || workspaces != current_workspaces {
                    eprintln!(
                        "[Watchdog] Active profile config changed. Old: {:?}, New: {:?}",
                        current_profile_name, profile_name
                    );

                    // 释放旧 Watcher
                    watcher = None;
                    debounce_map.clear();

                    current_profile_name = profile_name.clone();
                    current_workspaces = workspaces.clone();

                    if let Some(ref name) = profile_name {
                        if !workspaces.is_empty() {
                            let tx_clone = tx.clone();
                            match recommended_watcher(move |res| {
                                if let Ok(event) = res {
                                    let _ = tx_clone.send(event);
                                }
                            }) {
                                Ok(mut new_watcher) => {
                                    for ws in &workspaces {
                                        if ws.exists() {
                                            eprintln!("[Watchdog] Setting up notify watch for: {:?}", ws);
                                            if let Err(e) = new_watcher.watch(ws, RecursiveMode::Recursive) {
                                                eprintln!("[Watchdog] Error watching workspace {:?}: {}", ws, e);
                                            }
                                        }
                                    }
                                    watcher = Some(new_watcher);
                                    eprintln!("[Watchdog] File monitoring active for profile '{}'", name);
                                }
                                Err(e) => {
                                    eprintln!("[Watchdog] Failed to create notify watcher: {}", e);
                                }
                            }
                        }
                    }
                }
            }

            // 2. 消费来自 notify 的变动事件
            while let Ok(event) = rx.try_recv() {
                match event.kind {
                    notify::EventKind::Remove(_) | notify::EventKind::Create(_) | notify::EventKind::Modify(_) => {}
                    _ => continue, // 仅处理 Create/Modify/Remove 事件
                };

                let exclude_patterns = {
                    let guard = active_profile.read().unwrap();
                    if let Some(prof) = guard.as_ref() {
                        prof.exclude.clone()
                    } else {
                        Vec::new()
                    }
                };

                for path in event.paths {
                    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
                    if !crate::indexer::is_supported_extension(ext) {
                        continue;
                    }

                    if is_path_excluded(&path, &exclude_patterns) {
                        continue;
                    }

                    // 500ms 防抖静默期
                    let target_time = Instant::now() + Duration::from_millis(500);
                    debounce_map.insert(path, (target_time, event.kind));
                }
            }

            // 3. 检查是否有超出 500ms 防抖期的待处理任务
            let now = Instant::now();
            let mut expired_paths = Vec::new();
            for (path, (target_time, _)) in &debounce_map {
                if now >= *target_time {
                    expired_paths.push(path.clone());
                }
            }

            for path in expired_paths {
                if let Some((_, kind)) = debounce_map.remove(&path) {
                    let path_str = match path.to_str() {
                        Some(s) => s,
                        None => continue,
                    };

                    let (profile_name, max_lines) = {
                        let guard = active_profile.read().unwrap();
                        if let Some(prof) = guard.as_ref() {
                            (prof.name.clone(), prof.max_file_read_lines)
                        } else {
                            continue;
                        }
                    };

                    match kind {
                        notify::EventKind::Remove(_) => {
                            eprintln!("[Watchdog] File removed: {}", path_str);
                            if let Err(e) = db.delete_file(path_str) {
                                eprintln!("[Watchdog] Error deleting indexing for {}: {}", path_str, e);
                            }
                        }
                        notify::EventKind::Create(_) | notify::EventKind::Modify(_) => {
                            if path.exists() && path.is_file() {
                                eprintln!("[Watchdog] File changed: {}", path_str);
                                if let Err(e) = crate::indexer::process_file(&db, &profile_name, &path, path_str, max_lines) {
                                    eprintln!("[Watchdog] Error incremental indexing for {}: {}", path_str, e);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            // 轮询睡眠
            std::thread::sleep(Duration::from_millis(100));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceFolder;
    use crate::storage::CodeGraph;
    use std::fs::File;
    use std::io::Write;
    use std::sync::Arc;

    #[test]
    fn test_watchdog_incremental() {
        // 1. 创建临时目录
        let mut workspace_path = std::env::temp_dir();
        workspace_path.push(format!("dehydrator_watch_test_{}", std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&workspace_path).unwrap();

        // 2. 内存数据库
        let db = Arc::new(CodeGraph::open_in_memory().unwrap());

        // 3. 构建 Profile
        let profile = Profile {
            name: "watch-profile".to_string(),
            description: "Test".to_string(),
            workspaces: vec![WorkspaceFolder {
                path: workspace_path.clone(),
                tags: vec![],
            }],
            exclude: vec!["*.log".to_string()],
            max_file_read_lines: 10,
        };
        let active_profile = Arc::new(RwLock::new(Some(profile)));

        // 4. 启动 Watcher
        start_watcher(db.clone(), active_profile.clone());

        // 等待 Watcher 线程就绪并开始监听
        std::thread::sleep(Duration::from_millis(500));

        // 5. 新建测试文件
        let file_path = workspace_path.join("test_file.rs");
        let mut file = File::create(&file_path).unwrap();
        writeln!(file, "fn watch_test_func() {{ }}").unwrap();
        drop(file);

        // 等待事件通知 + 防抖 (500ms) + 线程缓冲
        std::thread::sleep(Duration::from_millis(2000));

        // 6. 验证数据库
        let path_str = file_path.to_str().unwrap();
        let file_rec_opt = db.get_file_by_path(path_str).unwrap();
        assert!(file_rec_opt.is_some(), "File should be indexed by watchdog");
        let file_rec = file_rec_opt.unwrap();
        let symbols = db.get_symbols_for_file(file_rec.id).unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "watch_test_func");

        // 7. 修改测试文件
        let mut file = File::create(&file_path).unwrap();
        writeln!(file, "fn watch_test_func_modified() {{ }}").unwrap();
        drop(file);

        std::thread::sleep(Duration::from_millis(2000));

        // 验证数据库内符号已被自动更新
        let symbols = db.get_symbols_for_file(file_rec.id).unwrap();
        assert_eq!(symbols.len(), 1);
        assert_eq!(symbols[0].name, "watch_test_func_modified");

        // 8. 删除测试文件
        std::fs::remove_file(&file_path).unwrap();

        std::thread::sleep(Duration::from_millis(2000));

        // 验证数据库内文件记录及关联符号被物理删除
        let file_rec_opt2 = db.get_file_by_path(path_str).unwrap();
        assert!(file_rec_opt2.is_none(), "File record should be deleted from DB on file deletion");

        // 清理临时目录
        let _ = std::fs::remove_dir_all(&workspace_path);
    }
}
