use rusqlite::{params, Connection, Result};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeFilter {
    Hourly,
    Daily,
    Monthly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TimeFrameFilter {
    Today,
    ThreeDays,
    OneWeek,
    FifteenDays,
    ThirtyDays,
    DateRange,
}

impl std::fmt::Display for TimeFrameFilter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Today => write!(f, "Today"),
            Self::ThreeDays => write!(f, "3 Days"),
            Self::OneWeek => write!(f, "1 Week"),
            Self::FifteenDays => write!(f, "15 Days"),
            Self::ThirtyDays => write!(f, "30 Days"),
            Self::DateRange => write!(f, "Date Range..."),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ChartDataPoint {
    pub label: String,      // 时间步标签 (刻度值)
    pub raw_val: f32,       // 原始累积天际线
    pub optimized_val: f32, // 优化后地板线
}


/// 线程安全的本地代码图谱（CodeGraph）数据库封装
pub struct CodeGraph {
    conn: Mutex<Connection>,
}

/// 文件元数据记录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub id: i64,
    pub profile_name: String,
    pub absolute_path: String,
    pub last_modified: i64,
}

/// AST 符号定义记录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRecord {
    pub id: i64,
    pub file_id: i64,
    pub name: String,
    pub kind: String, // "function", "class", "struct", "interface" 等
    pub start_line: i32,
    pub end_line: i32,
    pub signature: String,
}

/// 依赖关系记录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReferenceRecord {
    pub source_symbol_id: i64,
    pub target_symbol_name: String,
}

/// 用于批量插入的符号与引用数据
#[derive(Debug, Clone)]
pub struct SymbolData {
    pub name: String,
    pub kind: String,
    pub start_line: i32,
    pub end_line: i32,
    pub signature: String,
    pub references: Vec<String>,
}

impl CodeGraph {
    /// 打开本地文件数据库并初始化，同时启用外键约束
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        // 开启外键约束支持（SQLite 默认不开启级联删除等外键特性）
        conn.execute("PRAGMA foreign_keys = ON;", [])?;
        let _ = conn.execute("PRAGMA journal_mode = WAL;", []);
        let _ = conn.execute("PRAGMA synchronous = NORMAL;", []);
        
        let graph = Self {
            conn: Mutex::new(conn),
        };
        graph.init_tables()?;
        Ok(graph)
    }

    /// 创建内存数据库用于测试
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute("PRAGMA foreign_keys = ON;", [])?;
        
        let graph = Self {
            conn: Mutex::new(conn),
        };
        graph.init_tables()?;
        Ok(graph)
    }

    /// 初始化拓扑表及索引
    pub fn init_tables(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();

        // 1. files 表
        conn.execute(
            "CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                profile_name TEXT NOT NULL,
                absolute_path TEXT UNIQUE NOT NULL,
                last_modified INTEGER NOT NULL
            );",
            [],
        )?;

        // 2. symbols 表 (外键关联 files 且开启级联删除)
        conn.execute(
            "CREATE TABLE IF NOT EXISTS symbols (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                file_id INTEGER NOT NULL,
                name TEXT NOT NULL,
                kind TEXT NOT NULL,
                start_line INTEGER NOT NULL,
                end_line INTEGER NOT NULL,
                signature TEXT NOT NULL,
                FOREIGN KEY (file_id) REFERENCES files (id) ON DELETE CASCADE
            );",
            [],
        )?;

        // 3. references_graph 表 (联合主键，外键级联删除)
        conn.execute(
            "CREATE TABLE IF NOT EXISTS references_graph (
                source_symbol_id INTEGER NOT NULL,
                target_symbol_name TEXT NOT NULL,
                PRIMARY KEY (source_symbol_id, target_symbol_name),
                FOREIGN KEY (source_symbol_id) REFERENCES symbols (id) ON DELETE CASCADE
            );",
            [],
        )?;

        // 4. token_analytics 表
        conn.execute(
            "CREATE TABLE IF NOT EXISTS token_analytics (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                profile_name TEXT NOT NULL,
                agent_type TEXT NOT NULL,
                raw_tokens INTEGER NOT NULL,
                optimized_tokens INTEGER NOT NULL,
                timestamp DATETIME DEFAULT CURRENT_TIMESTAMP
            );",
            [],
        )?;

        // 创建辅助索引以提高查询性能
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_files_profile ON files (profile_name);",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_symbols_file_id ON symbols (file_id);",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols (name);",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_token_analytics_profile ON token_analytics (profile_name);",
            [],
        )?;

        Ok(())
    }

    /// 插入或更新文件元数据，并返回文件的 ID
    pub fn upsert_file(&self, profile_name: &str, absolute_path: &str, last_modified: i64) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO files (profile_name, absolute_path, last_modified)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(absolute_path) DO UPDATE SET
                 profile_name = excluded.profile_name,
                 last_modified = excluded.last_modified;",
            params![profile_name, absolute_path, last_modified],
        )?;

        let id = conn.query_row(
            "SELECT id FROM files WHERE absolute_path = ?1;",
            params![absolute_path],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// 删除指定路径的文件（关联的 symbols 和 references 会被级联删除）
    pub fn delete_file(&self, absolute_path: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM files WHERE absolute_path = ?1;",
            params![absolute_path],
        )?;
        Ok(())
    }

    /// 清空属于某个 Profile 的所有数据（文件、符号、引用等）
    pub fn delete_profile_data(&self, profile_name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM files WHERE profile_name = ?1;",
            params![profile_name],
        )?;
        Ok(())
    }

    /// 插入遥测 Token 流量统计记录
    pub fn insert_token_analytics(
        &self,
        profile_name: &str,
        agent_type: &str,
        raw_tokens: i64,
        optimized_tokens: i64,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO token_analytics (profile_name, agent_type, raw_tokens, optimized_tokens)
             VALUES (?1, ?2, ?3, ?4);",
            params![profile_name, agent_type, raw_tokens, optimized_tokens],
        )?;
        Ok(())
    }

    /// 获取特定 Profile 下的遥测 Token 时序统计
    pub fn get_token_analytics(
        &self,
        profile_name: &str,
        filter: TimeFilter,
    ) -> Result<Vec<ChartDataPoint>> {
        let conn = self.conn.lock().unwrap();
        let query = match filter {
            TimeFilter::Hourly => {
                "SELECT strftime('%H:00', timestamp, 'localtime') as bucket, SUM(raw_tokens), SUM(optimized_tokens)
                 FROM token_analytics
                 WHERE profile_name = ?1 AND timestamp >= datetime('now', '-24 hours')
                 GROUP BY bucket ORDER BY timestamp ASC;"
            }
            TimeFilter::Daily => {
                "SELECT strftime('%m-%d', timestamp, 'localtime') as bucket, SUM(raw_tokens), SUM(optimized_tokens)
                 FROM token_analytics
                 WHERE profile_name = ?1 AND timestamp >= datetime('now', '-30 days')
                 GROUP BY bucket ORDER BY timestamp ASC;"
            }
            TimeFilter::Monthly => {
                "SELECT strftime('%Y-%m', timestamp, 'localtime') as bucket, SUM(raw_tokens), SUM(optimized_tokens)
                 FROM token_analytics
                 WHERE profile_name = ?1
                 GROUP BY bucket ORDER BY timestamp ASC;"
            }
        };

        let mut stmt = conn.prepare(query)?;
        let rows = stmt.query_map(params![profile_name], |row| {
            let label: String = row.get(0)?;
            let raw_val: i64 = row.get(1)?;
            let optimized_val: i64 = row.get(2)?;
            Ok(ChartDataPoint {
                label,
                raw_val: raw_val as f32,
                optimized_val: optimized_val as f32,
            })
        })?;

        let mut list = Vec::new();
        for item in rows {
            list.push(item?);
        }
        Ok(list)
    }

    /// 获取特定 Profile 下的遥测 Token 时序统计 (v3 版本，支持自定义时间范围与聚合)
    pub fn get_token_analytics_v3(
        &self,
        profile_name: &str,
        filter: TimeFrameFilter,
        date_range: Option<(String, String)>,
    ) -> Result<Vec<ChartDataPoint>> {
        let conn = self.conn.lock().unwrap();
        let (query, has_range) = match filter {
            TimeFrameFilter::Today => (
                "SELECT strftime('%H:00', timestamp, 'localtime') as bucket, SUM(raw_tokens), SUM(optimized_tokens)
                 FROM token_analytics
                 WHERE profile_name = ?1 AND timestamp >= datetime('now', '-24 hours')
                 GROUP BY bucket ORDER BY timestamp ASC;".to_string(),
                false
            ),
            TimeFrameFilter::ThreeDays => (
                "SELECT strftime('%m-%d', timestamp, 'localtime') as bucket, SUM(raw_tokens), SUM(optimized_tokens)
                 FROM token_analytics
                 WHERE profile_name = ?1 AND timestamp >= datetime('now', '-3 days')
                 GROUP BY bucket ORDER BY timestamp ASC;".to_string(),
                false
            ),
            TimeFrameFilter::OneWeek => (
                "SELECT strftime('%m-%d', timestamp, 'localtime') as bucket, SUM(raw_tokens), SUM(optimized_tokens)
                 FROM token_analytics
                 WHERE profile_name = ?1 AND timestamp >= datetime('now', '-7 days')
                 GROUP BY bucket ORDER BY timestamp ASC;".to_string(),
                false
            ),
            TimeFrameFilter::FifteenDays => (
                "SELECT strftime('%m-%d', timestamp, 'localtime') as bucket, SUM(raw_tokens), SUM(optimized_tokens)
                 FROM token_analytics
                 WHERE profile_name = ?1 AND timestamp >= datetime('now', '-15 days')
                 GROUP BY bucket ORDER BY timestamp ASC;".to_string(),
                false
            ),
            TimeFrameFilter::ThirtyDays => (
                "SELECT strftime('%m-%d', timestamp, 'localtime') as bucket, SUM(raw_tokens), SUM(optimized_tokens)
                 FROM token_analytics
                 WHERE profile_name = ?1 AND timestamp >= datetime('now', '-30 days')
                 GROUP BY bucket ORDER BY timestamp ASC;".to_string(),
                false
            ),
            TimeFrameFilter::DateRange => (
                "SELECT strftime('%m-%d', timestamp, 'localtime') as bucket, SUM(raw_tokens), SUM(optimized_tokens)
                 FROM token_analytics
                 WHERE profile_name = ?1
                   AND date(timestamp, 'localtime') >= date(?2)
                   AND date(timestamp, 'localtime') <= date(?3)
                 GROUP BY bucket ORDER BY timestamp ASC;".to_string(),
                true
            ),
        };

        let mut list = Vec::new();
        if has_range {
            let mut stmt = conn.prepare(&query)?;
            let range = date_range.clone().unwrap_or(("".to_string(), "".to_string()));
            let rows = stmt.query_map(params![profile_name, range.0, range.1], |row| {
                let label: String = row.get(0)?;
                let raw_val: i64 = row.get(1)?;
                let optimized_val: i64 = row.get(2)?;
                Ok(ChartDataPoint {
                    label,
                    raw_val: raw_val as f32,
                    optimized_val: optimized_val as f32,
                })
            })?;
            for item in rows {
                list.push(item?);
            }
        } else {
            let mut stmt = conn.prepare(&query)?;
            let rows = stmt.query_map(params![profile_name], |row| {
                let label: String = row.get(0)?;
                let raw_val: i64 = row.get(1)?;
                let optimized_val: i64 = row.get(2)?;
                Ok(ChartDataPoint {
                    label,
                    raw_val: raw_val as f32,
                    optimized_val: optimized_val as f32,
                })
            })?;
            for item in rows {
                list.push(item?);
            }
        }
        Ok(list)
    }

    /// 清空指定文件的所有符号（方便文件修改后重新解析 AST 并覆盖）
    pub fn clear_file_symbols(&self, file_id: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM symbols WHERE file_id = ?1;",
            params![file_id],
        )?;
        Ok(())
    }

    /// 插入新的 AST 符号记录，返回新符号的 ID
    pub fn insert_symbol(
        &self,
        file_id: i64,
        name: &str,
        kind: &str,
        start_line: i32,
        end_line: i32,
        signature: &str,
    ) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO symbols (file_id, name, kind, start_line, end_line, signature)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6);",
            params![file_id, name, kind, start_line, end_line, signature],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// 插入或忽略依赖关系引用记录
    pub fn insert_reference(&self, source_symbol_id: i64, target_symbol_name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT OR IGNORE INTO references_graph (source_symbol_id, target_symbol_name)
             VALUES (?1, ?2);",
            params![source_symbol_id, target_symbol_name],
        )?;
        Ok(())
    }

    /// 根据文件绝对路径获取文件记录
    pub fn get_file_by_path(&self, absolute_path: &str) -> Result<Option<FileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, profile_name, absolute_path, last_modified FROM files WHERE absolute_path = ?1;",
        )?;
        let mut rows = stmt.query(params![absolute_path])?;
        if let Some(row) = rows.next()? {
            Ok(Some(FileRecord {
                id: row.get(0)?,
                profile_name: row.get(1)?,
                absolute_path: row.get(2)?,
                last_modified: row.get(3)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// 获取特定 Profile 下的所有文件记录
    pub fn get_files_by_profile(&self, profile_name: &str) -> Result<Vec<FileRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, profile_name, absolute_path, last_modified FROM files WHERE profile_name = ?1;",
        )?;
        let rows = stmt.query_map(params![profile_name], |row| {
            Ok(FileRecord {
                id: row.get(0)?,
                profile_name: row.get(1)?,
                absolute_path: row.get(2)?,
                last_modified: row.get(3)?,
            })
        })?;

        let mut list = Vec::new();
        for item in rows {
            list.push(item?);
        }
        Ok(list)
    }

    /// 获取特定文件的所有 AST 符号列表
    pub fn get_symbols_for_file(&self, file_id: i64) -> Result<Vec<SymbolRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, file_id, name, kind, start_line, end_line, signature FROM symbols WHERE file_id = ?1;",
        )?;
        let rows = stmt.query_map(params![file_id], |row| {
            Ok(SymbolRecord {
                id: row.get(0)?,
                file_id: row.get(1)?,
                name: row.get(2)?,
                kind: row.get(3)?,
                start_line: row.get(4)?,
                end_line: row.get(5)?,
                signature: row.get(6)?,
            })
        })?;

        let mut list = Vec::new();
        for item in rows {
            list.push(item?);
        }
        Ok(list)
    }

    /// 根据符号名跨工作空间查找符号定义及其关联文件
    pub fn find_symbol_definitions(&self, symbol_name: &str) -> Result<Vec<(FileRecord, SymbolRecord)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT f.id, f.profile_name, f.absolute_path, f.last_modified,
                    s.id, s.file_id, s.name, s.kind, s.start_line, s.end_line, s.signature
             FROM symbols s
             JOIN files f ON s.file_id = f.id
             WHERE s.name = ?1;",
        )?;

        let rows = stmt.query_map(params![symbol_name], |row| {
            let file = FileRecord {
                id: row.get(0)?,
                profile_name: row.get(1)?,
                absolute_path: row.get(2)?,
                last_modified: row.get(3)?,
            };
            let symbol = SymbolRecord {
                id: row.get(4)?,
                file_id: row.get(5)?,
                name: row.get(6)?,
                kind: row.get(7)?,
                start_line: row.get(8)?,
                end_line: row.get(9)?,
                signature: row.get(10)?,
            };
            Ok((file, symbol))
        })?;

        let mut list = Vec::new();
        for item in rows {
            list.push(item?);
        }
        Ok(list)
    }

    /// 获取特定符号的依赖引用名列表
    pub fn get_references_from_symbol(&self, source_symbol_id: i64) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT target_symbol_name FROM references_graph WHERE source_symbol_id = ?1;",
        )?;
        let rows = stmt.query_map(params![source_symbol_id], |row| row.get::<_, String>(0))?;

        let mut list = Vec::new();
        for item in rows {
            list.push(item?);
        }
        Ok(list)
    }

    /// 在单个数据库事务中保存文件的所有符号及其依赖引用
    pub fn save_file_symbols(
        &self,
        profile_name: &str,
        absolute_path: &str,
        last_modified: i64,
        symbols: Vec<SymbolData>,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        // 1. 插入或更新文件元数据
        tx.execute(
            "INSERT INTO files (profile_name, absolute_path, last_modified)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(absolute_path) DO UPDATE SET
                 profile_name = excluded.profile_name,
                 last_modified = excluded.last_modified;",
            params![profile_name, absolute_path, last_modified],
        )?;

        let file_id: i64 = tx.query_row(
            "SELECT id FROM files WHERE absolute_path = ?1;",
            params![absolute_path],
            |row| row.get(0),
        )?;

        // 2. 清空该文件原有的符号记录（外键级联删除 references_graph 表中的记录）
        tx.execute(
            "DELETE FROM symbols WHERE file_id = ?1;",
            params![file_id],
        )?;

        // 3. 批量插入符号和依赖关系
        for sym in symbols {
            tx.execute(
                "INSERT INTO symbols (file_id, name, kind, start_line, end_line, signature)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6);",
                params![
                    file_id,
                    sym.name,
                    sym.kind,
                    sym.start_line,
                    sym.end_line,
                    sym.signature
                ],
            )?;
            let sym_id = tx.last_insert_rowid();

            for ref_name in sym.references {
                tx.execute(
                    "INSERT OR IGNORE INTO references_graph (source_symbol_id, target_symbol_name)
                     VALUES (?1, ?2);",
                    params![sym_id, ref_name],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_code_graph_flow() {
        let graph = CodeGraph::open_in_memory().expect("Failed to create in-memory database");

        // 1. 测试插入文件
        let file_id = graph
            .upsert_file("dev-profile", "C:/src/lib.rs", 12345678)
            .expect("Failed to upsert file");
        
        let file_record = graph
            .get_file_by_path("C:/src/lib.rs")
            .expect("Failed to query file")
            .expect("File record missing");
        
        assert_eq!(file_record.id, file_id);
        assert_eq!(file_record.profile_name, "dev-profile");
        assert_eq!(file_record.last_modified, 12345678);

        // 测试唯一性及更新 (Conflict resolution)
        let new_file_id = graph
            .upsert_file("dev-profile", "C:/src/lib.rs", 87654321)
            .expect("Failed to update file");
        assert_eq!(file_id, new_file_id); // 应该保留相同的 ID

        let updated_record = graph
            .get_file_by_path("C:/src/lib.rs")
            .expect("Failed to query updated file")
            .unwrap();
        assert_eq!(updated_record.last_modified, 87654321);

        // 2. 测试插入符号
        let sym_id_1 = graph
            .insert_symbol(file_id, "my_func", "function", 10, 20, "fn my_func() -> i32")
            .expect("Failed to insert symbol 1");
        
        let _sym_id_2 = graph
            .insert_symbol(file_id, "MyStruct", "struct", 22, 35, "struct MyStruct { val: i32 }")
            .expect("Failed to insert symbol 2");

        let symbols = graph.get_symbols_for_file(file_id).expect("Failed to get symbols");
        assert_eq!(symbols.len(), 2);
        assert_eq!(symbols[0].name, "my_func");
        assert_eq!(symbols[1].name, "MyStruct");

        // 3. 测试插入引用关系
        graph.insert_reference(sym_id_1, "MyStruct").expect("Failed to insert reference");
        graph.insert_reference(sym_id_1, "ExternalType").expect("Failed to insert reference");

        let refs = graph.get_references_from_symbol(sym_id_1).expect("Failed to get references");
        assert_eq!(refs.len(), 2);
        assert!(refs.contains(&"MyStruct".to_string()));
        assert!(refs.contains(&"ExternalType".to_string()));

        // 4. 测试全局符号定义搜索
        let search_results = graph.find_symbol_definitions("my_func").expect("Failed to search symbols");
        assert_eq!(search_results.len(), 1);
        assert_eq!(search_results[0].0.absolute_path, "C:/src/lib.rs");
        assert_eq!(search_results[0].1.id, sym_id_1);

        // 5. 测试级联删除：删除文件时自动清除 symbols 和 references
        graph.delete_file("C:/src/lib.rs").expect("Failed to delete file");
        
        let missing_file = graph.get_file_by_path("C:/src/lib.rs").expect("Failed to query file");
        assert!(missing_file.is_none());

        let missing_symbols = graph.get_symbols_for_file(file_id).expect("Failed to query symbols after delete");
        assert!(missing_symbols.is_empty());

        let missing_refs = graph.get_references_from_symbol(sym_id_1).expect("Failed to query references after delete");
        assert!(missing_refs.is_empty());
    }

    #[test]
    fn test_delete_profile_data() {
        let graph = CodeGraph::open_in_memory().expect("Failed to create in-memory database");

        let file_id_1 = graph
            .upsert_file("profile-a", "C:/src/a.rs", 100)
            .expect("Failed to upsert file a");
        let file_id_2 = graph
            .upsert_file("profile-b", "C:/src/b.rs", 200)
            .expect("Failed to upsert file b");

        let _sym_id_1 = graph
            .insert_symbol(file_id_1, "func_a", "function", 1, 5, "fn func_a()")
            .expect("Failed to insert sym a");
        let _sym_id_2 = graph
            .insert_symbol(file_id_2, "func_b", "function", 1, 5, "fn func_b()")
            .expect("Failed to insert sym b");

        // 验证 get_files_by_profile
        let files_a = graph.get_files_by_profile("profile-a").expect("Failed to get files");
        assert_eq!(files_a.len(), 1);
        assert_eq!(files_a[0].absolute_path, "C:/src/a.rs");

        // 删除 profile-a 的数据
        graph.delete_profile_data("profile-a").expect("Failed to delete profile a data");

        // profile-a 对应的文件和符号应该被删除
        assert!(graph.get_file_by_path("C:/src/a.rs").unwrap().is_none());
        assert!(graph.get_symbols_for_file(file_id_1).unwrap().is_empty());

        // profile-b 对应的数据应该依然存在
        assert!(graph.get_file_by_path("C:/src/b.rs").unwrap().is_some());
        assert_eq!(graph.get_symbols_for_file(file_id_2).unwrap().len(), 1);
    }
}
