use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// 挂载的物理文件夹信息，包括文件物理路径和标签列表
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct WorkspaceFolder {
    pub path: PathBuf,
    pub tags: Vec<String>,
}

/// Profile 配置文件结构体，定义多工作空间和环境排除规则等
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub description: String,
    pub workspaces: Vec<WorkspaceFolder>,
    pub exclude: Vec<String>,
    pub max_file_read_lines: usize,
}

impl Profile {
    /// 从文件加载 Profile (支持 JSON / YAML/YML，默认解析为 YAML)
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self, Box<dyn std::error::Error>> {
        let path_ref = path.as_ref();
        let extension = path_ref
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_lowercase());

        let mut file = File::open(path_ref)?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;

        match extension.as_deref() {
            Some("json") => {
                let profile: Profile = serde_json::from_str(&content)?;
                Ok(profile)
            }
            _ => {
                let profile: Profile = serde_yaml::from_str(&content)?;
                Ok(profile)
            }
        }
    }

    /// 将 Profile 保存到指定文件 (支持 JSON / YAML/YML，默认保存为 YAML)
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<(), Box<dyn std::error::Error>> {
        let path_ref = path.as_ref();
        let extension = path_ref
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_lowercase());

        let content = match extension.as_deref() {
            Some("json") => serde_json::to_string_pretty(self)?,
            _ => serde_yaml::to_string(self)?,
        };

        let mut file = File::create(path_ref)?;
        file.write_all(content.as_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_yaml_serialization_deserialization() {
        let temp_dir = std::env::temp_dir();
        let config_path = temp_dir.join("test_profile.yaml");

        let profile = Profile {
            name: "test-rust-dev".to_string(),
            description: "Rust development environment".to_string(),
            workspaces: vec![
                WorkspaceFolder {
                    path: PathBuf::from("C:/Projects/rust_core"),
                    tags: vec!["core".to_string(), "backend".to_string()],
                },
                WorkspaceFolder {
                    path: PathBuf::from("D:/Projects/rust_ui"),
                    tags: vec!["ui".to_string(), "frontend".to_string()],
                },
            ],
            exclude: vec!["target/".to_string(), "*.log".to_string()],
            max_file_read_lines: 500,
        };

        // 保存文件
        profile.save_to_file(&config_path).expect("Failed to save YAML config");

        // 重新加载文件
        let loaded = Profile::load_from_file(&config_path).expect("Failed to load YAML config");

        assert_eq!(profile, loaded);

        // 清理临时文件
        let _ = fs::remove_file(config_path);
    }

    #[test]
    fn test_json_serialization_deserialization() {
        let temp_dir = std::env::temp_dir();
        let config_path = temp_dir.join("test_profile.json");

        let profile = Profile {
            name: "test-node-dev".to_string(),
            description: "Node.js development environment".to_string(),
            workspaces: vec![WorkspaceFolder {
                path: PathBuf::from("C:/Projects/node_app"),
                tags: vec!["node".to_string()],
            }],
            exclude: vec!["node_modules/".to_string()],
            max_file_read_lines: 1000,
        };

        // 保存文件
        profile.save_to_file(&config_path).expect("Failed to save JSON config");

        // 重新加载文件
        let loaded = Profile::load_from_file(&config_path).expect("Failed to load JSON config");

        assert_eq!(profile, loaded);

        // 清理临时文件
        let _ = fs::remove_file(config_path);
    }
}
