use crate::config::Profile;
use crate::storage::{CodeGraph, TimeFrameFilter, ChartDataPoint};
use crate::mcp::McpEvent;
use iced::widget::{button, column, container, row, scrollable, text, vertical_space, text_input};
use iced::Subscription;
use iced::font::Weight;
use iced::{Element, Length, Theme, Task, Event};
use std::sync::Arc;

static MCP_RECEIVER: std::sync::OnceLock<tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<McpEvent>>> = std::sync::OnceLock::new();

/// Elm-architecture 状态机消息
#[derive(Debug, Clone)]
pub enum UiMessage {
    /// 接收到来自 MCP 网关后台通道的消息
    McpEventReceived(McpEvent),
    /// 用户切换 Profile
    SelectProfile(String),
    /// 用户触发扫描某个 Profile
    ScanWorkspace(String),
    /// 扫描完成的回调（带状态）
    ScanFinished(Result<String, String>),
    /// 修改新建 Profile 的输入框文本
    NewProfileNameChanged(String),
    /// 新建 Profile 并切换为当前 Active
    CreateProfile(String),
    /// 触发原生 Windows 文件夹选择器，并将该路径加入 active profile
    AddFolderToProfile,
    /// 物理存盘所有 Profile
    SaveConfig,
    /// 全局事件（用于拦截按键缩放）
    EventOccurred(Event),
    /// 切换时序数据的时间过滤器
    ChangeTimeFrame(TimeFrameFilter),
    StartDateChanged(String),
    EndDateChanged(String),
    ApplyDateRange,
    /// 触发多智能体 Skill 注入
    TriggerSkillInjection,
}

pub struct DehydratorApp {
    db: Arc<CodeGraph>,
    profiles: Vec<Profile>,
    active_profile: Option<String>,
    mcp_profile_ref: Arc<std::sync::RwLock<Option<Profile>>>,
    total_token_saved: usize,
    system_logs: Vec<String>,
    new_profile_input: String,
    is_scanning: Option<String>,
    scale_factor: f32,
    active_filter: TimeFrameFilter,
    start_date_input: String,
    end_date_input: String,
    date_range_error: Option<String>,
    chart_data: Vec<ChartDataPoint>,
    canvas_cache: iced::widget::canvas::Cache,
}

impl DehydratorApp {
    pub fn new(
        db: Arc<CodeGraph>,
        profiles: Vec<Profile>,
        mcp_profile_ref: Arc<std::sync::RwLock<Option<Profile>>>,
    ) -> (Self, Task<UiMessage>) {
        let first_profile = profiles.first().cloned();
        let active_profile = first_profile.as_ref().map(|p| p.name.clone());
        if let Some(ref p) = first_profile {
            *mcp_profile_ref.write().unwrap() = Some(p.clone());
        }

        let initial_filter = TimeFrameFilter::Today;
        let initial_data = if let Some(ref name) = active_profile {
            db.get_token_analytics_v3(name, initial_filter, None).unwrap_or_default()
        } else {
            Vec::new()
        };

        (
            Self {
                db,
                profiles,
                active_profile,
                mcp_profile_ref,
                total_token_saved: 0,
                system_logs: vec![
                    "[SYSTEM] Dehydrator4Win UI dashboard initialized successfully.".to_string(),
                    "[SYSTEM] Ready for MCP client connection on 127.0.0.1:3001.".to_string(),
                ],
                new_profile_input: String::new(),
                is_scanning: None,
                scale_factor: 1.0,
                active_filter: initial_filter,
                start_date_input: String::new(),
                end_date_input: String::new(),
                date_range_error: None,
                chart_data: initial_data,
                canvas_cache: iced::widget::canvas::Cache::new(),
            },
            Task::none(),
        )
    }

    pub fn title(&self) -> String {
        String::from("Dehydrator4Win - Dark Context Dashboard")
    }

    pub fn theme(&self) -> Theme {
        Theme::Dark
    }

    fn reload_telemetry(&mut self) {
        if let Some(ref name) = self.active_profile {
            let date_range = if self.active_filter == TimeFrameFilter::DateRange {
                Some((self.start_date_input.clone(), self.end_date_input.clone()))
            } else {
                None
            };
            if let Ok(data) = self.db.get_token_analytics_v3(name, self.active_filter, date_range) {
                self.chart_data = data;
                self.canvas_cache.clear();
            }
        }
    }

    pub fn update(&mut self, message: UiMessage) -> Task<UiMessage> {
        match message {
            UiMessage::McpEventReceived(event) => {
                match event {
                    McpEvent::TokenSaved { path, saved_tokens } => {
                        self.total_token_saved += saved_tokens;
                        self.system_logs.push(format!(
                            "[INTERCEPTED] Dehydrated {} - saved estimated {} tokens.",
                            path, saved_tokens
                        ));
                    }
                    McpEvent::Log(msg) => {
                        self.system_logs.push(msg);
                    }
                }
                // 限制内存中的运行日志行数，避免无限内存占用
                if self.system_logs.len() > 150 {
                    self.system_logs.remove(0);
                }
                self.reload_telemetry();
            }
            UiMessage::SelectProfile(name) => {
                if let Some(profile) = self.profiles.iter().find(|p| p.name == name) {
                    self.active_profile = Some(name.clone());
                    *self.mcp_profile_ref.write().unwrap() = Some(profile.clone());
                    self.system_logs.push(format!(
                        "[SYSTEM] Dynamically switched active profile to '{}' (context redirected).",
                        name
                    ));
                    self.reload_telemetry();
                }
            }
            UiMessage::ScanWorkspace(name) => {
                if let Some(profile) = self.profiles.iter().find(|p| p.name == name).cloned() {
                    let db = self.db.clone();
                    let active_profile_name = name.clone();
                    self.system_logs.push(format!(
                        "[SYSTEM] Initiating parallel workspace scan for '{}'...",
                        name
                    ));
                    self.is_scanning = Some(name.clone());

                    let indexer = crate::indexer::Indexer::new(db);
                    return Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                indexer.scan_profile(&profile).map_err(|e| e.to_string())
                            })
                            .await
                        },
                        move |res| {
                            match res {
                                Ok(Ok(())) => UiMessage::ScanFinished(Ok(active_profile_name.clone())),
                                Ok(Err(e)) => UiMessage::ScanFinished(Err(e)),
                                Err(e) => UiMessage::ScanFinished(Err(e.to_string())),
                            }
                        }
                    );
                }
            }
            UiMessage::ScanFinished(res) => {
                self.is_scanning = None;
                match res {
                    Ok(name) => {
                        self.system_logs.push(format!(
                            "[SYSTEM] Parallel workspace scan for '{}' completed successfully! CodeGraph updated.",
                            name
                        ));
                    }
                    Err(err) => {
                        self.system_logs.push(format!(
                            "[ERROR] Parallel workspace scan failed: {}",
                            err
                        ));
                    }
                }
            }
            UiMessage::NewProfileNameChanged(name) => {
                self.new_profile_input = name;
            }
            UiMessage::CreateProfile(name) => {
                let name = name.trim().to_string();
                if !name.is_empty() {
                    if !self.profiles.iter().any(|p| p.name == name) {
                        let new_profile = Profile {
                            name: name.clone(),
                            description: format!("Workspace profile for {}", name),
                            workspaces: vec![],
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
                            max_file_read_lines: 100,
                        };
                        self.profiles.push(new_profile.clone());
                        self.active_profile = Some(name.clone());
                        *self.mcp_profile_ref.write().unwrap() = Some(new_profile);
                        self.system_logs.push(format!("[SYSTEM] Created new workspace profile '{}'.", name));
                        self.new_profile_input.clear();
                        self.reload_telemetry();
                        // 自动触发物理存盘
                        return Task::done(UiMessage::SaveConfig);
                    } else {
                        self.system_logs.push(format!("[WARNING] Profile '{}' already exists.", name));
                    }
                }
            }
            UiMessage::AddFolderToProfile => {
                if let Some(active_name) = &self.active_profile {
                    // 调用 native-dialog 触发 Windows 原生选择文件夹
                    match native_dialog::FileDialog::new().show_open_single_dir() {
                        Ok(Some(path)) => {
                            if let Some(profile) = self.profiles.iter_mut().find(|p| p.name == *active_name) {
                                if !profile.workspaces.iter().any(|w| w.path == path) {
                                    profile.workspaces.push(crate::config::WorkspaceFolder {
                                        path: path.clone(),
                                        tags: vec![],
                                    });
                                    *self.mcp_profile_ref.write().unwrap() = Some(profile.clone());
                                    self.system_logs.push(format!(
                                        "[SYSTEM] Added directory '{:?}' to profile '{}'.",
                                        path, active_name
                                    ));
                                    // 自动触发物理存盘
                                    return Task::done(UiMessage::SaveConfig);
                                } else {
                                    self.system_logs.push(format!(
                                        "[WARNING] Directory '{:?}' is already in profile '{}'.",
                                        path, active_name
                                    ));
                                }
                            }
                        }
                        Ok(None) => {
                            self.system_logs.push("[SYSTEM] Folder selection cancelled by user.".to_string());
                        }
                        Err(e) => {
                            self.system_logs.push(format!("[ERROR] Folder dialog error: {}", e));
                        }
                    }
                } else {
                    self.system_logs.push("[WARNING] No active profile selected to add directory to.".to_string());
                }
            }
            UiMessage::SaveConfig => {
                let config_dir = std::path::PathBuf::from("config");
                if let Err(e) = std::fs::create_dir_all(&config_dir) {
                    self.system_logs.push(format!("[ERROR] Failed to create config dir: {}", e));
                    return Task::none();
                }

                let mut success = true;
                for p in &self.profiles {
                    let file_path = config_dir.join(format!("{}.yaml", p.name));
                    if let Err(e) = p.save_to_file(&file_path) {
                        self.system_logs.push(format!("[ERROR] Failed to save profile '{}': {}", p.name, e));
                        success = false;
                    }
                }

                if success {
                    self.system_logs.push("[SYSTEM] All configuration profiles saved to YAML successfully.".to_string());
                }
            }
            UiMessage::EventOccurred(event) => {
                if let iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) = event {
                    if modifiers.control() {
                        match key.as_ref() {
                            iced::keyboard::Key::Character("=") | iced::keyboard::Key::Character("+") => {
                                self.scale_factor = (self.scale_factor + 0.1).min(2.5);
                                self.system_logs.push(format!("[SYSTEM] Zoom in: scale factor set to {:.1}", self.scale_factor));
                            }
                            iced::keyboard::Key::Character("-") => {
                                self.scale_factor = (self.scale_factor - 0.1).max(0.6);
                                self.system_logs.push(format!("[SYSTEM] Zoom out: scale factor set to {:.1}", self.scale_factor));
                            }
                            iced::keyboard::Key::Character("0") => {
                                self.scale_factor = 1.0;
                                self.system_logs.push("[SYSTEM] Reset zoom: scale factor set to 1.0".to_string());
                            }
                            _ => {}
                        }
                    }
                }
            }
            UiMessage::ChangeTimeFrame(filter) => {
                self.active_filter = filter;
                self.date_range_error = None;
                self.reload_telemetry();
            }
            UiMessage::StartDateChanged(val) => {
                self.start_date_input = val;
            }
            UiMessage::EndDateChanged(val) => {
                self.end_date_input = val;
            }
            UiMessage::ApplyDateRange => {
                let date_regex = regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap();
                if !date_regex.is_match(&self.start_date_input) || !date_regex.is_match(&self.end_date_input) {
                    self.date_range_error = Some("Format must be YYYY-MM-DD".to_string());
                } else {
                    self.date_range_error = None;
                    self.reload_telemetry();
                }
            }
            UiMessage::TriggerSkillInjection => {
                if let Some(ref name) = self.active_profile {
                    if let Some(profile) = self.profiles.iter().find(|p| p.name == *name) {
                        let mut success_count = 0;
                        for ws in &profile.workspaces {
                            if let Err(e) = crate::indexer::inject_skills(&ws.path) {
                                self.system_logs.push(format!(
                                    "[ERROR] Failed to inject skills to {:?}: {}",
                                    ws.path, e
                                ));
                            } else {
                                success_count += 1;
                                self.system_logs.push(format!(
                                    "[SYSTEM] AI skills injected successfully into workspace: {:?}",
                                    ws.path
                                ));
                            }
                        }
                        if success_count > 0 {
                            self.system_logs.push(format!(
                                "[SYSTEM] Skill injection complete. Target folders: .codex, .claude, .gemini, .agent"
                            ));
                        }
                    }
                }
            }
        }
        Task::none()
    }

    pub fn subscription(&self) -> Subscription<UiMessage> {
        Subscription::batch(vec![
            Subscription::run(Self::mcp_event_stream),
            iced::event::listen().map(UiMessage::EventOccurred),
        ])
    }

    fn mcp_event_stream() -> impl iced::futures::Stream<Item = UiMessage> {
        iced::stream::channel(100, |mut output| async move {
            if let Some(mutex_rx) = MCP_RECEIVER.get() {
                let mut rx = mutex_rx.lock().await;
                while let Some(event) = rx.recv().await {
                    use iced::futures::sink::SinkExt;
                    let _ = output.send(UiMessage::McpEventReceived(event)).await;
                }
            } else {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        })
    }

    pub fn view(&self) -> Element<'_, UiMessage> {
        let total_raw_sum: f32 = self.chart_data.iter().map(|p| p.raw_val).sum();
        let total_opt_sum: f32 = self.chart_data.iter().map(|p| p.optimized_val).sum();
        let saved_margin = total_raw_sum - total_opt_sum;

        let efficiency_ratio = if total_raw_sum > 0.0 {
            (saved_margin / total_raw_sum) * 100.0
        } else {
            0.0
        };

        // --- 1. 构建 Scrollable Profiles 列表卡片 ---
        let mut profiles_col = column![].spacing(10);

        for p in &self.profiles {
            let is_active = self.active_profile.as_ref() == Some(&p.name);
            let is_scanning_this = self.is_scanning.as_ref() == Some(&p.name);

            // 标题和状态小绿点组件
            let title_row = if is_active {
                row![
                    container(column![])
                        .width(6)
                        .height(6)
                        .style(|_| container::Style {
                            background: Some(iced::Background::Color(iced::Color::from_rgb(0.0, 1.0, 0.5))),
                            border: iced::Border {
                                radius: 3.0.into(),
                                ..Default::default()
                            },
                            ..Default::default()
                        }),
                    text(&p.name)
                        .size(13)
                        .font(iced::Font { weight: Weight::Bold, ..Default::default() })
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center)
            } else {
                row![
                    text(&p.name)
                        .size(13)
                        .font(iced::Font { weight: Weight::Bold, ..Default::default() })
                ]
                .spacing(6)
                .align_y(iced::Alignment::Center)
            };

            // 扁平的标题切换按钮
            let title_btn = button(title_row)
                .padding(0)
                .style(move |_theme, _status| {
                    iced::widget::button::Style {
                        background: Some(iced::Background::Color(iced::Color::TRANSPARENT)),
                        border: iced::Border {
                            width: 0.0,
                            ..Default::default()
                        },
                        text_color: if is_active {
                            iced::Color::from_rgb(0.0, 1.0, 0.5)
                        } else {
                            iced::Color::from_rgb(0.8, 0.8, 0.8)
                        },
                        ..Default::default()
                    }
                })
                .on_press(UiMessage::SelectProfile(p.name.clone()));

            // Scan Ghost 按钮
            let scan_btn = if is_scanning_this {
                button(
                    text("Scanning...")
                        .size(10)
                        .font(iced::Font { weight: Weight::Bold, ..Default::default() })
                )
                .padding(iced::Padding { top: 4.0, right: 8.0, bottom: 4.0, left: 8.0 })
                .style(|_theme, _status| {
                    iced::widget::button::Style {
                        background: Some(iced::Background::Color(iced::Color::TRANSPARENT)),
                        border: iced::Border {
                            color: iced::Color::from_rgb(0.4, 0.4, 0.4),
                            width: 1.0,
                            radius: 4.0.into(),
                        },
                        text_color: iced::Color::from_rgb(0.6, 0.6, 0.6),
                        ..Default::default()
                    }
                })
            } else {
                button(
                    text("Scan")
                        .size(10)
                        .font(iced::Font { weight: Weight::Bold, ..Default::default() })
                )
                .padding(iced::Padding { top: 4.0, right: 8.0, bottom: 4.0, left: 8.0 })
                .style(|_theme, status| {
                    iced::widget::button::Style {
                        background: Some(iced::Background::Color(iced::Color::TRANSPARENT)),
                        border: iced::Border {
                            color: match status {
                                iced::widget::button::Status::Hovered => iced::Color::from_rgb(1.0, 0.8, 0.0),
                                _ => iced::Color::from_rgb(0.4, 0.35, 0.15),
                            },
                            width: 1.0,
                            radius: 4.0.into(),
                        },
                        text_color: match status {
                            iced::widget::button::Status::Hovered => iced::Color::from_rgb(1.0, 0.8, 0.0),
                            _ => iced::Color::from_rgb(0.8, 0.7, 0.4),
                        },
                        ..Default::default()
                    }
                })
                .on_press(UiMessage::ScanWorkspace(p.name.clone()))
            };

            let card_header = row![
                title_btn,
                iced::widget::horizontal_space(),
                scan_btn
            ]
            .width(Length::Fill)
            .align_y(iced::Alignment::Center);

            let mut card_col = column![card_header].spacing(8);

            // 如果是活动 Profile 展开列出包含目录与 Add Directory 按钮
            if is_active {
                let mut workspaces_col = column![].spacing(6).padding(iced::Padding {
                    top: 2.0,
                    right: 0.0,
                    bottom: 2.0,
                    left: 12.0,
                });

                if p.workspaces.is_empty() {
                    workspaces_col = workspaces_col.push(
                        text("No directories bound.")
                            .size(11)
                            .color(iced::Color::from_rgb(0.5, 0.5, 0.5))
                    );
                } else {
                    for ws in &p.workspaces {
                        let display_path = ws.path.file_name()
                            .and_then(|n| n.to_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| ws.path.to_string_lossy().into_owned());
                        workspaces_col = workspaces_col.push(
                            row![
                                text("•")
                                    .size(11)
                                    .color(iced::Color::from_rgb(0.0, 1.0, 0.5)),
                                text(display_path)
                                    .size(11)
                                    .color(iced::Color::from_rgb(0.65, 0.65, 0.65))
                            ]
                            .spacing(5)
                        );
                    }
                }

                // Add Directory 宽度撑满的 ghost 按钮
                let add_dir_btn = button(
                    text("+ Add Directory")
                        .size(11)
                        .font(iced::Font { weight: Weight::Bold, ..Default::default() })
                )
                .padding(iced::Padding { top: 8.0, right: 10.0, bottom: 8.0, left: 10.0 })
                .width(Length::Fill)
                .style(|_theme, status| {
                    iced::widget::button::Style {
                        background: match status {
                            iced::widget::button::Status::Hovered => Some(iced::Background::Color(iced::Color::from_rgba(0.0, 0.8, 1.0, 0.08))),
                            _ => Some(iced::Background::Color(iced::Color::TRANSPARENT)),
                        },
                        border: iced::Border {
                            color: match status {
                                iced::widget::button::Status::Hovered => iced::Color::from_rgb(0.0, 0.8, 1.0),
                                _ => iced::Color::from_rgb(0.15, 0.35, 0.45),
                            },
                            width: 1.0,
                            radius: 4.0.into(),
                        },
                        text_color: match status {
                            iced::widget::button::Status::Hovered => iced::Color::from_rgb(0.0, 0.8, 1.0),
                            _ => iced::Color::from_rgb(0.0, 0.6, 0.8),
                        },
                        ..Default::default()
                    }
                })
                .on_press(UiMessage::AddFolderToProfile);

                card_col = card_col.push(workspaces_col).push(add_dir_btn);
            }

            // 整合的卡片容器组件
            let card = container(card_col)
                .padding(12)
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(iced::Background::Color(iced::Color::from_rgb8(34, 37, 41))),
                    border: iced::Border {
                        color: if is_active {
                            iced::Color::from_rgb(0.0, 1.0, 0.5) // Active Accent Highlight
                        } else {
                            iced::Color::from_rgb8(45, 48, 53)
                        },
                        width: if is_active { 1.5 } else { 1.0 },
                        radius: 6.0.into(),
                    },
                    shadow: iced::Shadow::default(),
                    text_color: None,
                });

            profiles_col = profiles_col.push(card);
        }

        // --- 2. 左侧 Sidebar 布局 ---
        let sidebar_content = column![
            text("DEHYDRATOR4WIN")
                .size(18)
                .font(iced::Font { weight: Weight::Bold, ..Default::default() })
                .color(iced::Color::from_rgb(0.0, 1.0, 0.5)),
            text("Non-Chromium AI Context OS")
                .size(10)
                .color(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            vertical_space().height(25),
            text("WORKSPACE PROFILES")
                .size(11)
                .font(iced::Font { weight: Weight::Bold, ..Default::default() })
                .color(iced::Color::from_rgb(0.6, 0.6, 0.6)),
            vertical_space().height(5),
            // Profile 统一输入行组合
            row![
                text_input("New profile...", &self.new_profile_input)
                    .on_input(UiMessage::NewProfileNameChanged)
                    .on_submit(UiMessage::CreateProfile(self.new_profile_input.clone()))
                    .padding(8)
                    .size(12)
                    .style(|theme, status| {
                        let mut base = iced::widget::text_input::default(theme, status);
                        base.border.radius = iced::border::Radius::new(0.0).left(4.0);
                        base.border.width = 1.0;
                        base.border.color = match status {
                            iced::widget::text_input::Status::Focused => iced::Color::from_rgb(0.0, 1.0, 0.5),
                            _ => iced::Color::from_rgb8(48, 51, 56),
                        };
                        base
                    }),
                button(
                    text("+")
                        .size(12)
                        .font(iced::Font { weight: Weight::Bold, ..Default::default() })
                )
                .padding(iced::Padding { top: 8.0, right: 12.0, bottom: 8.0, left: 12.0 })
                .style(|_theme, status| {
                    iced::widget::button::Style {
                        background: match status {
                            iced::widget::button::Status::Hovered => Some(iced::Background::Color(iced::Color::from_rgb8(0, 180, 90))),
                            _ => Some(iced::Background::Color(iced::Color::from_rgb8(48, 51, 56))),
                        },
                        border: iced::Border {
                            width: 0.0,
                            radius: iced::border::Radius::new(0.0).right(4.0),
                            ..Default::default()
                        },
                        text_color: iced::Color::from_rgb(0.9, 0.9, 0.9),
                        ..Default::default()
                    }
                })
                .on_press(UiMessage::CreateProfile(self.new_profile_input.clone()))
            ]
            .spacing(0)
            .align_y(iced::Alignment::Center),
            vertical_space().height(15),
            scrollable(profiles_col).height(Length::Fill)
        ]
        .spacing(5);

        let sidebar = container(sidebar_content)
            .width(260)
            .height(Length::Fill)
            .padding(20)
            .style(|_| container::Style {
                background: Some(iced::Background::Color(iced::Color::from_rgb8(24, 25, 28))),
                border: iced::Border {
                    color: iced::Color::from_rgb8(38, 41, 46),
                    width: 1.0,
                    radius: 0.0.into(),
                },
                shadow: iced::Shadow::default(),
                text_color: None,
            });

        // --- 3. 右侧顶部 Token 看板 ---
        let token_saved_text: iced::widget::Text<'_, iced::Theme, iced::Renderer> = text(format!("{}", self.total_token_saved))
            .size(54)
            .font(iced::Font { weight: Weight::Bold, ..Default::default() })
            .color(iced::Color::from_rgb(0.0, 1.0, 0.4));

        let activate_skills_btn = button(
            text("ACTIVATE AI SKILLS")
                .size(12)
                .font(iced::Font { weight: Weight::Bold, ..Default::default() })
        )
        .padding(iced::Padding { top: 10.0, right: 16.0, bottom: 10.0, left: 16.0 })
        .style(|_theme, status| {
            iced::widget::button::Style {
                background: match status {
                    iced::widget::button::Status::Hovered => Some(iced::Background::Color(iced::Color::from_rgba(0.0, 1.0, 0.5, 0.15))),
                    _ => Some(iced::Background::Color(iced::Color::from_rgba(0.0, 1.0, 0.5, 0.05))),
                },
                border: iced::Border {
                    color: match status {
                        iced::widget::button::Status::Hovered => iced::Color::from_rgb(0.0, 1.0, 0.5),
                        _ => iced::Color::from_rgba(0.0, 1.0, 0.5, 0.4),
                    },
                    width: 1.5,
                    radius: 6.0.into(),
                },
                text_color: iced::Color::from_rgb(0.0, 1.0, 0.5),
                ..Default::default()
            }
        })
        .on_press(UiMessage::TriggerSkillInjection);

        let h_style = |f| if self.active_filter == f { iced::Color::from_rgb(0.0, 1.0, 0.5) } else { iced::Color::WHITE };

        let period_selectors = row![
            button(text("24H").size(10)).style(move |_,_| iced::widget::button::Style { text_color: h_style(TimeFrameFilter::Today), ..Default::default() }).on_press(UiMessage::ChangeTimeFrame(TimeFrameFilter::Today)).padding(5),
            button(text("3D").size(10)).style(move |_,_| iced::widget::button::Style { text_color: h_style(TimeFrameFilter::ThreeDays), ..Default::default() }).on_press(UiMessage::ChangeTimeFrame(TimeFrameFilter::ThreeDays)).padding(5),
            button(text("7D").size(10)).style(move |_,_| iced::widget::button::Style { text_color: h_style(TimeFrameFilter::OneWeek), ..Default::default() }).on_press(UiMessage::ChangeTimeFrame(TimeFrameFilter::OneWeek)).padding(5),
            button(text("15D").size(10)).style(move |_,_| iced::widget::button::Style { text_color: h_style(TimeFrameFilter::FifteenDays), ..Default::default() }).on_press(UiMessage::ChangeTimeFrame(TimeFrameFilter::FifteenDays)).padding(5),
            button(text("30D").size(10)).style(move |_,_| iced::widget::button::Style { text_color: h_style(TimeFrameFilter::ThirtyDays), ..Default::default() }).on_press(UiMessage::ChangeTimeFrame(TimeFrameFilter::ThirtyDays)).padding(5),
        ].spacing(4);

        let token_saved_box = container(
            row![
                column![
                    text("TOTAL TOKENS INTERCEPTED").size(11).font(iced::Font { weight: Weight::Bold, ..Default::default() }).color(iced::Color::from_rgb(0.65, 0.65, 0.65)),
                    vertical_space().height(4),
                    token_saved_text,
                    vertical_space().height(2),
                    text("Bypassed redundancy metrics via native code dehydration.").size(10).color(iced::Color::from_rgb(0.5, 0.5, 0.5)),
                ].width(Length::FillPortion(4)),
                
                column![
                    text(format!("Raw Overhead Ceiling: {:.0}", total_raw_sum)).size(11).color(iced::Color::from_rgb(0.5, 0.7, 1.0)),
                    text(format!("Dehydrated Spent Floor: {:.0}", total_opt_sum)).size(11).color(iced::Color::from_rgb(0.0, 1.0, 0.5)),
                    text(format!("Net Intercept Efficiency: {:.1}%", efficiency_ratio)).size(11).color(iced::Color::from_rgb(1.0, 0.8, 0.0)),
                ].width(Length::FillPortion(3)).spacing(6),

                column![
                    period_selectors,
                    vertical_space().height(12),
                    activate_skills_btn
                ]
                .width(Length::FillPortion(3))
                .align_x(iced::Alignment::End)
            ]
            .align_y(iced::Alignment::Center)
        )
        .padding(20)
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(iced::Background::Color(iced::Color::from_rgb8(34, 37, 41))),
            border: iced::Border {
                color: iced::Color::from_rgb8(45, 48, 53),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..Default::default()
        });

        // --- 3.5. 遥测图表组件 ---
        let chart_header = row![
            text("TOKEN TELEMETRY ANALYTICS")
                .size(11)
                .font(iced::Font { weight: Weight::Bold, ..Default::default() })
                .color(iced::Color::from_rgb(0.5, 0.5, 0.5)),
        ]
        .align_y(iced::Alignment::Center)
        .width(Length::Fill);

        let canvas_widget = iced::widget::canvas(crate::chart::TelemetryCanvas::new(&self.canvas_cache, &self.chart_data))
            .width(Length::Fill)
            .height(200);

        let chart_box = container(
            column![
                chart_header,
                vertical_space().height(10),
                canvas_widget
            ]
        )
        .padding(15)
        .width(Length::Fill)
        .style(|_| container::Style {
            background: Some(iced::Background::Color(iced::Color::from_rgb8(34, 37, 41))),
            border: iced::Border {
                color: iced::Color::from_rgb8(45, 48, 53),
                width: 1.0,
                radius: 8.0.into(),
            },
            shadow: iced::Shadow::default(),
            text_color: None,
        });

        // --- 4. 右侧中下日志终端 ---
        let mut log_col = column![].spacing(3);
        for log in &self.system_logs {
            let color = if log.contains("[INTERCEPTED]") {
                iced::Color::from_rgb(0.0, 1.0, 0.5)
            } else if log.contains("[ERROR]") {
                iced::Color::from_rgb(1.0, 0.3, 0.3)
            } else if log.contains("[SYSTEM]") {
                iced::Color::from_rgb(0.2, 0.6, 1.0)
            } else {
                iced::Color::from_rgb(0.5, 0.7, 0.5)
            };

            log_col = log_col.push(
                text(log)
                    .size(12)
                    .font(iced::Font::MONOSPACE)
                    .color(color)
                    .width(Length::Fill)
            );
        }

        let log_box = container(
            scrollable(
                container(log_col.width(Length::Fill))
                    .width(Length::Fill)
                    .padding(iced::Padding {
                        top: 10.0,
                        bottom: 10.0,
                        left: 15.0,
                        right: 15.0,
                    })
            )
            .direction(scrollable::Direction::Vertical(
                scrollable::Scrollbar::new().width(6).scroller_width(6)
            ))
            .height(Length::Fill)
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .style(|_| container::Style {
            background: Some(iced::Background::Color(iced::Color::from_rgb8(18, 19, 21))),
            border: iced::Border {
                color: iced::Color::from_rgb8(34, 37, 41),
                width: 1.0,
                radius: 6.0.into(),
            },
            shadow: iced::Shadow::default(),
            text_color: None,
        });

        let main_content = column![
            token_saved_box,
            vertical_space().height(15),
            chart_box,
            vertical_space().height(15),
            text("STREAMING GATEWAY AUDIT LOGS")
                    .size(11)
                    .font(iced::Font { weight: Weight::Bold, ..Default::default() })
                    .color(iced::Color::from_rgb(0.5, 0.5, 0.5)),
            vertical_space().height(5),
            log_box,
        ]
        .padding(20)
        .width(Length::Fill)
        .height(Length::Fill);

        let main_container = container(main_content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(iced::Background::Color(iced::Color::from_rgb8(30, 32, 35))),
                ..Default::default()
            });

        row![sidebar, main_container]
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}



/// 启动 Iced 渲染面板的外部接口。接管当前主线程。
pub fn run_ui(
    db: Arc<CodeGraph>,
    profiles: Vec<Profile>,
    mcp_profile_ref: Arc<std::sync::RwLock<Option<Profile>>>,
    mcp_rx: tokio::sync::mpsc::UnboundedReceiver<McpEvent>,
) -> Result<(), iced::Error> {
    let _ = MCP_RECEIVER.set(tokio::sync::Mutex::new(mcp_rx));
    iced::application(
        DehydratorApp::title,
        DehydratorApp::update,
        DehydratorApp::view,
    )
    .window(iced::window::Settings {
        position: iced::window::Position::Centered,
        ..Default::default()
    })
    .subscription(DehydratorApp::subscription)
    .theme(DehydratorApp::theme)
    .scale_factor(move |state| state.scale_factor as f64)
    .run_with(move || DehydratorApp::new(db, profiles, mcp_profile_ref))
}
