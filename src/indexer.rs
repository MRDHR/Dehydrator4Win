use crate::config::Profile;
use crate::storage::CodeGraph;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use regex::Regex;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

/// 提取出的符号元数据
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSymbol {
    pub name: String,
    pub kind: String, // "function", "struct", "class", "interface" 等
    pub start_line: usize,
    pub end_line: usize,
    pub signature: String,
}

struct LangRegexes {
    patterns: Vec<(Regex, &'static str)>,
}

fn get_lang_regexes(ext: &str) -> &'static LangRegexes {
    static RUST_RE: OnceLock<LangRegexes> = OnceLock::new();
    static GO_RE: OnceLock<LangRegexes> = OnceLock::new();
    static JS_TS_RE: OnceLock<LangRegexes> = OnceLock::new();
    static PYTHON_RE: OnceLock<LangRegexes> = OnceLock::new();
    static GENERIC_RE: OnceLock<LangRegexes> = OnceLock::new();

    match ext {
        "rs" => RUST_RE.get_or_init(|| LangRegexes {
            patterns: vec![
                (Regex::new(r#"^\s*(?:pub(?:\([^)]+\))?\s+)?(?:async\s+)?(?:const\s+)?(?:unsafe\s+)?(?:extern\s+(?:"[^"]+"\s+)?)?fn\s+(\w+)"#).unwrap(), "function"),
                (Regex::new(r"^\s*(?:pub(?:\([^)]+\))?\s+)?struct\s+(\w+)").unwrap(), "struct"),
                (Regex::new(r"^\s*(?:pub(?:\([^)]+\))?\s+)?enum\s+(\w+)").unwrap(), "struct"),
                (Regex::new(r"^\s*(?:pub(?:\([^)]+\))?\s+)?trait\s+(\w+)").unwrap(), "interface"),
            ]
        }),
        "go" => GO_RE.get_or_init(|| LangRegexes {
            patterns: vec![
                (Regex::new(r"^\s*func\s+(?:\([^)]+\)\s+)?(\w+)").unwrap(), "function"),
                (Regex::new(r"^\s*type\s+(\w+)\s+struct").unwrap(), "struct"),
                (Regex::new(r"^\s*type\s+(\w+)\s+interface").unwrap(), "interface"),
            ]
        }),
        "js" | "ts" | "jsx" | "tsx" => JS_TS_RE.get_or_init(|| LangRegexes {
            patterns: vec![
                (Regex::new(r"^\s*(?:export\s+)?(?:default\s+)?(?:async\s+)?function\s+(\w+)").unwrap(), "function"),
                (Regex::new(r"^\s*(?:export\s+)?(?:const|let|var)\s+(\w+)\s*=\s*(?:async\s*)?\(.*?\)\s*=>").unwrap(), "function"),
                (Regex::new(r"^\s*(?:export\s+)?(?:default\s+)?class\s+(\w+)").unwrap(), "class"),
                (Regex::new(r"^\s*(?:export\s+)?interface\s+(\w+)").unwrap(), "interface"),
            ]
        }),
        "py" => PYTHON_RE.get_or_init(|| LangRegexes {
            patterns: vec![
                (Regex::new(r"^\s*def\s+(\w+)").unwrap(), "function"),
                (Regex::new(r"^\s*class\s+(\w+)").unwrap(), "class"),
            ]
        }),
        _ => GENERIC_RE.get_or_init(|| LangRegexes {
            patterns: vec![
                (Regex::new(r#"^\s*(?:public|private|protected|static|final|abstract\s+)*class\s+(\w+)"#).unwrap(), "class"),
                (Regex::new(r#"^\s*(?:public|private|protected|static|final|abstract\s+)*interface\s+(\w+)"#).unwrap(), "interface"),
                (Regex::new(r#"^\s*(?:public|private|protected|static\s+)*struct\s+(\w+)"#).unwrap(), "struct"),
                (Regex::new(r#"^\s*(?:[a-zA-Z_<>\d::*&]+\s+)+(\w+)\s*\([^)]*\)"#).unwrap(), "function"),
            ]
        }),
    }
}

/// 针对大括号型语言，清洗行内的注释和字符串字面量以保证括号匹配精度
fn clean_line_for_braces(line: &str) -> String {
    // 1. 移除单行注释 //
    let without_comment = if let Some(idx) = line.find("//") {
        &line[..idx]
    } else {
        line
    };

    // 2. 移除双引号字符串 (处理转义字符)
    let re_str = Regex::new(r#""[^"\\]*(?:\\.[^"\\]*)*""#).unwrap();
    let without_str = re_str.replace_all(without_comment, "\"\"");

    // 3. 移除单引号字符 (处理转义字符)
    let re_char = Regex::new(r#"'[^'\\]*(?:\\.[^'\\]*)*'"#).unwrap();
    re_char.replace_all(&without_str, "''").into_owned()
}

/// 代码脱水算法：按行读取文件，将大括号或缩进内函数体抹除，仅保留签名，返回脱水后的文本与提取出的符号列表。
pub fn generate_skeleton_by_regex(content: &str, ext: &str) -> (String, Vec<ParsedSymbol>) {
    if ext == "py" {
        generate_skeleton_python(content)
    } else {
        generate_skeleton_curly_brace(content, ext)
    }
}

fn generate_skeleton_curly_brace(content: &str, ext: &str) -> (String, Vec<ParsedSymbol>) {
    let mut output = String::new();
    let mut symbols = Vec::new();

    struct DehydratingFunc {
        name: String,
        kind: String,
        start_line: usize,
        signature: String,
        brace_count: usize,
        has_hidden_placeholder: bool,
    }

    struct PendingFunc {
        name: String,
        kind: String,
        start_line: usize,
        signature: String,
        lines: Vec<String>,
    }

    struct ActiveContainer {
        brace_level_at_start: usize,
        has_opened: bool,
        symbol_index: usize,
    }

    let mut dehydrating_func: Option<DehydratingFunc> = None;
    let mut pending_func: Option<PendingFunc> = None;
    let mut active_containers: Vec<ActiveContainer> = Vec::new();

    let mut global_brace_level = 0;
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let raw_line = lines[i];
        let cleaned = clean_line_for_braces(raw_line);
        let open_braces = cleaned.matches('{').count();
        let close_braces = cleaned.matches('}').count();

        // 1. 如果当前处于 pending_func 状态
        if let Some(mut pending) = pending_func.take() {
            pending.lines.push(raw_line.to_string());
            if open_braces > 0 {
                if open_braces > close_braces {
                    let brace_diff = open_braces - close_braces;
                    dehydrating_func = Some(DehydratingFunc {
                        name: pending.name,
                        kind: pending.kind,
                        start_line: pending.start_line,
                        signature: pending.signature,
                        brace_count: brace_diff,
                        has_hidden_placeholder: false,
                    });
                } else {
                    symbols.push(ParsedSymbol {
                        name: pending.name,
                        kind: pending.kind,
                        start_line: pending.start_line,
                        end_line: i + 1,
                        signature: pending.signature,
                    });
                }
                for line in pending.lines {
                    output.push_str(&line);
                    output.push('\n');
                }
            } else {
                if pending.lines.len() > 5 {
                    for line in pending.lines {
                        output.push_str(&line);
                        output.push('\n');
                    }
                } else {
                    pending_func = Some(pending);
                }
            }
            i += 1;
            continue;
        }

        // 2. 如果当前处于 dehydrating_func 状态
        if let Some(mut func) = dehydrating_func.take() {
            func.brace_count = (func.brace_count + open_braces).saturating_sub(close_braces);
            if func.brace_count == 0 {
                symbols.push(ParsedSymbol {
                    name: func.name,
                    kind: func.kind,
                    start_line: func.start_line,
                    end_line: i + 1,
                    signature: func.signature,
                });
                output.push_str(raw_line);
                output.push('\n');
            } else {
                if !func.has_hidden_placeholder {
                    let indent = raw_line.chars().take_while(|c| c.is_whitespace()).collect::<String>();
                    output.push_str(&format!("{}// [Implementation hidden by Dehydrator4Win to save Token]\n", indent));
                    func.has_hidden_placeholder = true;
                }
                dehydrating_func = Some(func);
            }
            i += 1;
            continue;
        }

        // 3. 处于 Normal 状态下的常规处理
        for container in &mut active_containers {
            if !container.has_opened && open_braces > 0 {
                container.has_opened = true;
            }
        }

        let lang_config = get_lang_regexes(ext);
        let mut matched = false;

        for (re, kind) in &lang_config.patterns {
            if let Some(caps) = re.captures(raw_line) {
                if let Some(name_match) = caps.get(caps.len() - 1) {
                    let name = name_match.as_str().to_string();
                    let signature = raw_line.trim().to_string();
                    let start_line = i + 1;

                    if *kind == "function" && (name == "if" || name == "for" || name == "while" || name == "switch" || name == "catch") {
                        continue;
                    }

                    if *kind == "function" {
                        if open_braces > 0 {
                            if open_braces > close_braces {
                                let brace_diff = open_braces - close_braces;
                                dehydrating_func = Some(DehydratingFunc {
                                    name,
                                    kind: kind.to_string(),
                                    start_line,
                                    signature,
                                    brace_count: brace_diff,
                                    has_hidden_placeholder: false,
                                });
                            } else {
                                symbols.push(ParsedSymbol {
                                    name,
                                    kind: kind.to_string(),
                                    start_line,
                                    end_line: start_line,
                                    signature,
                                });
                            }
                        } else {
                            pending_func = Some(PendingFunc {
                                name,
                                kind: kind.to_string(),
                                start_line,
                                signature,
                                lines: vec![raw_line.to_string()],
                            });
                        }
                    } else {
                        let symbol_index = symbols.len();
                        symbols.push(ParsedSymbol {
                            name: name.clone(),
                            kind: kind.to_string(),
                            start_line,
                            end_line: start_line,
                            signature: signature.clone(),
                        });
                        active_containers.push(ActiveContainer {
                            brace_level_at_start: global_brace_level,
                            has_opened: open_braces > 0,
                            symbol_index,
                        });
                    }
                    output.push_str(raw_line);
                    output.push('\n');
                    matched = true;
                    break;
                }
            }
        }

        if !matched {
            output.push_str(raw_line);
            output.push('\n');
        }

        global_brace_level = (global_brace_level + open_braces).saturating_sub(close_braces);

        let mut still_active = Vec::new();
        for container in active_containers {
            if container.has_opened && global_brace_level <= container.brace_level_at_start {
                symbols[container.symbol_index].end_line = i + 1;
            } else {
                still_active.push(container);
            }
        }
        active_containers = still_active;

        i += 1;
    }

    if let Some(func) = dehydrating_func {
        symbols.push(ParsedSymbol {
            name: func.name,
            kind: func.kind,
            start_line: func.start_line,
            end_line: lines.len(),
            signature: func.signature,
        });
    }
    if let Some(pending) = pending_func {
        for line in pending.lines {
            output.push_str(&line);
            output.push('\n');
        }
    }
    for container in active_containers {
        symbols[container.symbol_index].end_line = lines.len();
    }

    (output, symbols)
}

fn generate_skeleton_python(content: &str) -> (String, Vec<ParsedSymbol>) {
    let mut output = String::new();
    let mut symbols = Vec::new();

    struct DehydratingFuncPy {
        name: String,
        kind: String,
        start_line: usize,
        signature: String,
        base_indent: usize,
        has_hidden_placeholder: bool,
        last_non_empty_line: usize,
    }

    struct ActiveContainerPy {
        base_indent: usize,
        symbol_index: usize,
    }

    let mut dehydrating_func: Option<DehydratingFuncPy> = None;
    let mut active_containers: Vec<ActiveContainerPy> = Vec::new();

    let mut last_non_empty_line = 0;
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;

    while i < lines.len() {
        let raw_line = lines[i];
        let is_empty = raw_line.trim().is_empty();
        let is_comment = raw_line.trim().starts_with('#');

        if let Some(mut func) = dehydrating_func.take() {
            if is_empty || is_comment {
                output.push_str(raw_line);
                output.push('\n');
                dehydrating_func = Some(func);
            } else {
                let current_indent = raw_line.chars().take_while(|c| c.is_whitespace() || *c == '\t').count();
                if current_indent > func.base_indent {
                    func.last_non_empty_line = i + 1;
                    if !func.has_hidden_placeholder {
                        let indent = " ".repeat(func.base_indent + 4);
                        output.push_str(&format!("{}# [Implementation hidden by Dehydrator4Win to save Token]\n", indent));
                        func.has_hidden_placeholder = true;
                    }
                    dehydrating_func = Some(func);
                } else {
                    symbols.push(ParsedSymbol {
                        name: func.name,
                        kind: func.kind,
                        start_line: func.start_line,
                        end_line: func.last_non_empty_line,
                        signature: func.signature,
                    });
                    dehydrating_func = None;
                    i -= 1;
                }
            }
            i += 1;
            continue;
        }

        if !is_empty && !is_comment {
            let current_indent = raw_line.chars().take_while(|c| c.is_whitespace() || *c == '\t').count();
            let mut still_active = Vec::new();
            for container in active_containers {
                if current_indent <= container.base_indent {
                    symbols[container.symbol_index].end_line = last_non_empty_line;
                } else {
                    still_active.push(container);
                }
            }
            active_containers = still_active;
        }

        if !is_empty && !is_comment {
            last_non_empty_line = i + 1;
        }

        let mut matched = false;
        if !is_empty && !is_comment {
            let lang_config = get_lang_regexes("py");
            for (re, kind) in &lang_config.patterns {
                if let Some(caps) = re.captures(raw_line) {
                    if let Some(name_match) = caps.get(1) {
                        let name = name_match.as_str().to_string();
                        let signature = raw_line.trim().to_string();
                        let start_line = i + 1;
                        let base_indent = raw_line.chars().take_while(|c| c.is_whitespace() || *c == '\t').count();

                        if *kind == "function" {
                            dehydrating_func = Some(DehydratingFuncPy {
                                name,
                                kind: kind.to_string(),
                                start_line,
                                signature,
                                base_indent,
                                has_hidden_placeholder: false,
                                last_non_empty_line: start_line,
                            });
                        } else {
                            let symbol_index = symbols.len();
                            symbols.push(ParsedSymbol {
                                name: name.clone(),
                                kind: kind.to_string(),
                                start_line,
                                end_line: start_line,
                                signature: signature.clone(),
                            });
                            active_containers.push(ActiveContainerPy {
                                base_indent,
                                symbol_index,
                            });
                        }
                        output.push_str(raw_line);
                        output.push('\n');
                        matched = true;
                        break;
                    }
                }
            }
        }

        if !matched {
            output.push_str(raw_line);
            output.push('\n');
        }

        i += 1;
    }

    if let Some(func) = dehydrating_func {
        symbols.push(ParsedSymbol {
            name: func.name,
            kind: func.kind,
            start_line: func.start_line,
            end_line: func.last_non_empty_line,
            signature: func.signature,
        });
    }
    for container in active_containers {
        symbols[container.symbol_index].end_line = last_non_empty_line;
    }

    (output, symbols)
}


/// 判断是否是关键字
fn is_not_keyword(word: &str) -> bool {
    static KEYWORDS: OnceLock<HashSet<&'static str>> = OnceLock::new();
    let kw = KEYWORDS.get_or_init(|| {
        let list = vec![
            "fn", "pub", "use", "mod", "struct", "enum", "trait", "impl", "let", "mut", "if", "else", "match",
            "for", "in", "while", "loop", "return", "break", "continue", "async", "await", "const", "static",
            "unsafe", "where", "type", "as", "self", "Self", "true", "false", "import", "from", "class", "def",
            "pass", "func", "interface", "package", "var", "function", "export", "default", "void", "int",
            "double", "float", "char", "string", "bool", "usize", "u32", "u64", "i32", "i64", "String", "Vec",
            "Option", "Result", "Some", "None", "Ok", "Err",
        ];
        list.into_iter().collect()
    });
    !kw.contains(word) && word.len() > 1
}

fn is_supported_extension(ext: &str) -> bool {
    let ext_lower = ext.to_lowercase();
    match ext_lower.as_str() {
        "rs" | "go" | "js" | "ts" | "jsx" | "tsx" | "py" | "java" | "kt" |
        "c" | "cpp" | "cc" | "cxx" | "h" | "hpp" | "cs" | "rb" | "php" | "swift" |
        "sh" | "bat" | "ps1" | "html" | "css" | "json" | "yaml" | "yml" | "toml" |
        "md" | "txt" | "xml" | "properties" | "gradle" | "sql" => true,
        _ => false,
    }
}

/// 文件处理函数，用于从磁盘提取符号并写入数据库
fn process_file(
    db: &CodeGraph,
    profile_name: &str,
    path: &Path,
    path_str: &str,
    _max_file_read_lines: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    if !is_supported_extension(ext) {
        return Ok(());
    }

    let metadata = fs::metadata(path)?;
    if metadata.len() > 1_024_000 {
        return Ok(()); // Skip files larger than 1MB
    }

    let last_modified = metadata
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_secs() as i64;

    let existing_file = db.get_file_by_path(path_str)?;
    let need_update = match &existing_file {
        None => true,
        Some(record) => last_modified > record.last_modified,
    };

    if need_update {
        let content = match fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                if e.kind() == std::io::ErrorKind::InvalidData {
                    return Ok(()); // Gracefully ignore non-UTF-8 files
                }
                return Err(e.into());
            }
        };
        let (_, parsed_symbols) = generate_skeleton_by_regex(&content, ext);

        let file_id = db.upsert_file(profile_name, path_str, last_modified)?;
        db.clear_file_symbols(file_id)?;

        let content_lines: Vec<&str> = content.lines().collect();
        let word_re = Regex::new(r"\b[a-zA-Z_]\w*\b")?;

        for sym in parsed_symbols {
            let sym_id = db.insert_symbol(
                file_id,
                &sym.name,
                &sym.kind,
                sym.start_line as i32,
                sym.end_line as i32,
                &sym.signature,
            )?;

            // 分析代码体提取单词依赖
            if sym.start_line > 0 && sym.end_line >= sym.start_line && sym.end_line <= content_lines.len() {
                let mut words_in_sym = HashSet::new();
                for line_idx in (sym.start_line - 1)..sym.end_line {
                    let line = content_lines[line_idx];
                    for cap in word_re.find_iter(line) {
                        let word = cap.as_str();
                        if word != sym.name {
                            words_in_sym.insert(word.to_string());
                        }
                    }
                }

                for target_name in words_in_sym {
                    if is_not_keyword(&target_name) {
                        db.insert_reference(sym_id, &target_name)?;
                    }
                }
            }
        }
    }

    Ok(())
}

/// 索引器结构
pub struct Indexer {
    db: Arc<CodeGraph>,
}

impl Indexer {
    pub fn new(db: Arc<CodeGraph>) -> Self {
        Self { db }
    }

    /// 多线程增量扫描工作空间下的代码文件
    pub fn scan_profile(&self, profile: &Profile) -> Result<(), Box<dyn std::error::Error>> {
        for workspace in &profile.workspaces {
            if !workspace.path.exists() {
                continue;
            }

            let mut builder = WalkBuilder::new(&workspace.path);
            
            // 配置全局 exclude 过滤
            let mut override_builder = OverrideBuilder::new(&workspace.path);
            for pattern in &profile.exclude {
                let glob_pattern = format!("!{}", pattern);
                if let Err(err) = override_builder.add(&glob_pattern) {
                    eprintln!("Error adding override pattern {}: {}", pattern, err);
                }
            }

            if let Ok(overrides) = override_builder.build() {
                builder.overrides(overrides);
            }

            let walker = builder.build_parallel();
            let db = self.db.clone();
            let profile_name = profile.name.clone();
            let max_lines = profile.max_file_read_lines;

            walker.run(move || {
                let db = db.clone();
                let profile_name = profile_name.clone();
                Box::new(move |result| {
                    if let Ok(entry) = result {
                        let path = entry.path();
                        if path.is_file() {
                            if let Some(path_str) = path.to_str() {
                                if let Err(err) = process_file(&db, &profile_name, path, path_str, max_lines) {
                                    eprintln!("Error processing file {}: {}", path_str, err);
                                }
                            }
                        }
                    }
                    ignore::WalkState::Continue
                })
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WorkspaceFolder;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn test_skeleton_curly_brace() {
        let rust_code = r#"
// 这是一个测试注释
pub fn hello_world(x: i32) -> i32 {
    let y = x + 1;
    println!("y: {}", y);
    y
}

struct MyData {
    val: String,
}
"#;
        let (dehydrated, symbols) = generate_skeleton_by_regex(rust_code, "rs");
        
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "hello_world");
        assert_eq!(symbols[0].kind, "function");
        assert_eq!(symbols[0].start_line, 3);
        assert_eq!(symbols[0].end_line, 7);
        assert_eq!(symbols[0].signature, "pub fn hello_world(x: i32) -> i32 {");

        assert_eq!(symbols[1].name, "MyData");
        assert_eq!(symbols[1].kind, "struct");
        
        assert!(dehydrated.contains("pub fn hello_world(x: i32) -> i32 {"));
        assert!(dehydrated.contains("// [Implementation hidden by Dehydrator4Win to save Token]"));
        assert!(!dehydrated.contains("let y = x + 1;"));
    }

    #[test]
    fn test_skeleton_python() {
        let py_code = r#"
class Calculator:
    def add(self, a, b):
        # 这是一个加法
        result = a + b
        return result

    def sub(self, a, b):
        return a - b
"#;
        let (dehydrated, symbols) = generate_skeleton_by_regex(py_code, "py");

        assert_eq!(symbols.len(), 3);
        assert_eq!(symbols[0].name, "Calculator");
        assert_eq!(symbols[0].kind, "class");
        
        assert_eq!(symbols[1].name, "add");
        assert_eq!(symbols[1].kind, "function");
        assert_eq!(symbols[1].start_line, 3);
        assert_eq!(symbols[1].end_line, 6);

        assert!(dehydrated.contains("class Calculator:"));
        assert!(dehydrated.contains("def add(self, a, b):"));
        assert!(dehydrated.contains("# [Implementation hidden by Dehydrator4Win to save Token]"));
        assert!(!dehydrated.contains("result = a + b"));
    }

    #[test]
    fn test_indexer_scan_integration() {
        // 创建临时工作区目录
        let mut workspace_path = std::env::temp_dir();
        workspace_path.push(format!("dehydrator_test_{}", std::time::SystemTime::now().duration_since(std::time::SystemTime::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&workspace_path).unwrap();

        // 写入测试代码文件
        let file_a_path = workspace_path.join("a.rs");
        let mut file_a = File::create(&file_a_path).unwrap();
        writeln!(
            file_a,
            "fn first_func() {{ println!(\"first\"); }} \n fn second_func() {{ first_func(); }}"
        )
        .unwrap();

        // 创建内存数据库
        let db = Arc::new(CodeGraph::open_in_memory().unwrap());
        let indexer = Indexer::new(db.clone());

        // 构造 Profile
        let profile = Profile {
            name: "test-profile".to_string(),
            description: "Test".to_string(),
            workspaces: vec![WorkspaceFolder {
                path: workspace_path.clone(),
                tags: vec![],
            }],
            exclude: vec!["*.log".to_string()],
            max_file_read_lines: 10,
        };

        // 执行扫描
        indexer.scan_profile(&profile).expect("Scan failed");

        // 从数据库进行断言
        let path_str = file_a_path.to_str().unwrap();
        let file_rec = db.get_file_by_path(path_str).unwrap().unwrap();
        assert_eq!(file_rec.profile_name, "test-profile");

        let symbols = db.get_symbols_for_file(file_rec.id).unwrap();
        assert_eq!(symbols.len(), 2);
        
        let names: HashSet<String> = symbols.iter().map(|s| s.name.clone()).collect();
        assert!(names.contains("first_func"));
        assert!(names.contains("second_func"));

        // 验证依赖关系（second_func 调用了 first_func）
        let second_sym = symbols.iter().find(|s| s.name == "second_func").unwrap();
        let refs = db.get_references_from_symbol(second_sym.id).unwrap();
        assert!(refs.contains(&"first_func".to_string()));

        // 清理临时目录
        let _ = std::fs::remove_dir_all(&workspace_path);
    }
}
