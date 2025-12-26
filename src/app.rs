use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::SystemTime;

use crate::indexer::{FileIndexer, IndexBuildStats, IndexRootSource};
use crate::searcher::{MatchType, SearchResult, Searcher};

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Search,
    Settings,
}

#[derive(PartialEq, Clone, Copy)]
enum FileTypeFilter {
    All,
    Files,
    Folders,
    Documents,
    Images,
    Videos,
    Audio,
}

pub struct FileSearchApp {
    search_text: String,
    searcher: Searcher,
    indexer: Arc<Mutex<FileIndexer>>,
    results: Arc<Mutex<Vec<SearchResult>>>,
    selected_result: Option<usize>,
    current_tab: Tab,
    index_paths: Vec<PathBuf>,
    is_indexing: bool,
    index_progress: (usize, usize),
    total_files: usize,
    show_hidden: bool,
    window_size: [f32; 2],
    file_extension: String,  // 文件扩展名过滤
    file_type_filter: FileTypeFilter,  // 文件类型过滤
    new_path_input: String,  // 新路径输入
    index_seq: Arc<AtomicU64>,
    search_seq: Arc<AtomicU64>,
    last_index_time: Option<SystemTime>,
    index_stats: Arc<Mutex<Option<IndexBuildStats>>>,
    #[cfg(windows)]
    is_elevated: Option<bool>,
    #[cfg(windows)]
    admin_prompt_once: bool,
    #[cfg(windows)]
    admin_prompt_open: bool,
}

impl Default for FileSearchApp {
    fn default() -> Self {
        let indexer = FileIndexer::new();

        // 默认索引路径：Windows 自动枚举全部磁盘；非 Windows 使用根目录
        let index_paths = FileSearchApp::default_index_paths();

        Self {
            search_text: String::new(),
            searcher: Searcher::new(),
            indexer: Arc::new(Mutex::new(indexer)),
            results: Arc::new(Mutex::new(Vec::new())),
            selected_result: None,
            current_tab: Tab::Search,
            index_paths,
            is_indexing: false,
            index_progress: (0, 0),
            total_files: 0,
            show_hidden: false,
            window_size: [800.0, 600.0],
            file_extension: String::new(),
            file_type_filter: FileTypeFilter::All,
            new_path_input: String::new(),
            index_seq: Arc::new(AtomicU64::new(0)),
            search_seq: Arc::new(AtomicU64::new(0)),
            last_index_time: None,
            index_stats: Arc::new(Mutex::new(None)),
            #[cfg(windows)]
            is_elevated: None,
            #[cfg(windows)]
            admin_prompt_once: false,
            #[cfg(windows)]
            admin_prompt_open: false,
        }
    }
}

impl FileSearchApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();
        // 不使用本地缓存：启动后直接从 NTFS 的 USN/MFT 枚举构建索引（失败则回退 WalkDir 扫描）。
        app.rebuild_index();

        app
    }

    fn default_index_paths() -> Vec<PathBuf> {
        #[cfg(windows)]
        {
            let mut paths = Vec::new();
            for letter in b'A'..=b'Z' {
                let drive = format!("{}:\\", letter as char);
                let path = PathBuf::from(&drive);
                if path.is_dir() {
                    paths.push(path);
                }
            }
            if paths.is_empty() {
                vec![PathBuf::from(".")]
            } else {
                paths
            }
        }

        #[cfg(not(windows))]
        {
            vec![PathBuf::from("/")]
        }
    }

    fn open_path_in_os(path: &str) {
        let open_path = if cfg!(windows) {
            path.replace("/", "\\")
        } else {
            path.to_string()
        };

        if opener::open(&open_path).is_ok() {
            return;
        }

        if cfg!(windows) {
            let _ = std::process::Command::new("cmd")
                .args(["/c", "start", "", &open_path])
                .spawn();
            return;
        }

        let _ = std::process::Command::new("xdg-open").arg(&open_path).spawn();
    }

    fn rebuild_index(&mut self) {
        let indexer = Arc::clone(&self.indexer);
        let paths = self.index_paths.clone();
        let index_seq = Arc::clone(&self.index_seq);
        let seq = index_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let index_stats = Arc::clone(&self.index_stats);

        let handles = {
            let indexer_guard = indexer.lock().unwrap();
            indexer_guard.begin_indexing();
            indexer_guard.handles()
        };

        thread::spawn(move || {
            #[cfg(windows)]
            {
                let _ = crate::windows_usn::try_enable_usn_privileges();
            }
            let (entries, usn_states, stats) =
                FileIndexer::build_index_snapshot_with_stats(paths, Some(&handles));
            if index_seq.load(Ordering::SeqCst) != seq {
                return;
            }
            {
                // 先更新内存索引，让搜索尽快可用；缓存写入放到后面，不阻塞“索引完成”的体验
                let mut indexer_guard = indexer.lock().unwrap();
                indexer_guard.replace_index(entries, usn_states);
            }
            let mut guard = index_stats.lock().unwrap();
            *guard = Some(stats);
        });
        self.last_index_time = Some(SystemTime::now());
    }

    fn perform_search(&mut self) {
        let search_text = self.search_text.clone();
        let indexer = Arc::clone(&self.indexer);
        let results = Arc::clone(&self.results);
        let search_options = self.searcher.options.clone();
        let file_type_filter = self.file_type_filter;
        let file_extension = self.file_extension.clone();
        let search_seq = Arc::clone(&self.search_seq);
        let seq = search_seq.fetch_add(1, Ordering::SeqCst) + 1;

        thread::spawn(move || {
            let indexer_guard = indexer.lock().unwrap();
            let mut searcher = Searcher::new();
            searcher.set_options(search_options);
            let mut search_results = searcher.search(&*indexer_guard, &search_text);

            // 应用文件类型过滤
            if file_type_filter != FileTypeFilter::All || !file_extension.is_empty() {
                search_results.retain(|r| {
                    let entry = &r.entry;

                    // 文件夹过滤
                    if file_type_filter == FileTypeFilter::Folders && entry.is_dir {
                        return true;
                    }
                    if file_type_filter == FileTypeFilter::Folders && !entry.is_dir {
                        return false;
                    }
                    if file_type_filter == FileTypeFilter::Files && entry.is_dir {
                        return false;
                    }

                    // 文件类型过滤
                    if !entry.is_dir {
                        let ext = entry.name.split('.').last().unwrap_or("").to_lowercase();

                        match file_type_filter {
                            FileTypeFilter::Documents => {
                                let docs = ["doc", "docx", "txt", "pdf", "xls", "xlsx", "ppt", "pptx", "md"];
                                if !docs.contains(&ext.as_str()) && !entry.is_dir {
                                    return false;
                                }
                            }
                            FileTypeFilter::Images => {
                                let images = ["jpg", "jpeg", "png", "gif", "bmp", "svg", "webp", "ico"];
                                if !images.contains(&ext.as_str()) && !entry.is_dir {
                                    return false;
                                }
                            }
                            FileTypeFilter::Videos => {
                                let videos = ["mp4", "avi", "mkv", "mov", "wmv", "flv", "webm"];
                                if !videos.contains(&ext.as_str()) && !entry.is_dir {
                                    return false;
                                }
                            }
                            FileTypeFilter::Audio => {
                                let audio = ["mp3", "wav", "flac", "aac", "ogg", "wma", "m4a"];
                                if !audio.contains(&ext.as_str()) && !entry.is_dir {
                                    return false;
                                }
                            }
                            _ => {}
                        }

                        // 扩展名过滤
                        if !file_extension.is_empty() {
                            let target_ext = file_extension.trim_start_matches('.').to_lowercase();
                            if ext != target_ext {
                                return false;
                            }
                        }
                    }

                    true
                });
            }

            if search_seq.load(Ordering::SeqCst) != seq {
                return;
            }

            let mut results_guard = results.lock().unwrap();
            *results_guard = search_results;
        });
        self.selected_result = None;
    }

    fn format_size(size: u64) -> String {
        if size == u64::MAX {
            return "—".to_string();
        }
        if size < 1024 {
            format!("{} B", size)
        } else if size < 1024 * 1024 {
            format!("{:.1} KB", size as f64 / 1024.0)
        } else if size < 1024 * 1024 * 1024 {
            format!("{:.1} MB", size as f64 / (1024.0 * 1024.0))
        } else {
            format!("{:.2} GB", size as f64 / (1024.0 * 1024.0 * 1024.0))
        }
    }
}

impl eframe::App for FileSearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 检查索引状态
        {
            let indexer = self.indexer.lock().unwrap();
            self.is_indexing = indexer.is_indexing();
            self.index_progress = indexer.progress();
            self.total_files = indexer.get_entries().len();
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            // 顶部标签页
            ui.horizontal(|ui| {
                if ui.selectable_label(self.current_tab == Tab::Search, "搜索").clicked() {
                    self.current_tab = Tab::Search;
                }
                if ui.selectable_label(self.current_tab == Tab::Settings, "设置").clicked() {
                    self.current_tab = Tab::Settings;
                }
                ui.separator();
                ui.label(format!("文件数: {}", self.total_files));
                if self.is_indexing {
                    ui.label(egui::RichText::new("索引中...").color(egui::Color32::from_rgb(255, 180, 0)));
                }
            });

            ui.separator();

            match self.current_tab {
                Tab::Search => self.show_search_tab(ui),
                Tab::Settings => self.show_settings_tab(ui),
            }
        });

        #[cfg(windows)]
        {
            // 若 USN 枚举因权限(code=5)回退到 WalkDir，则主动提示一次可重启为管理员。
            if !self.admin_prompt_once {
                let elevated = match self.is_elevated {
                    Some(v) => v,
                    None => {
                        let v = crate::windows_usn::is_process_elevated().unwrap_or(false);
                        self.is_elevated = Some(v);
                        v
                    }
                };

                if !elevated {
                    if let Some(stats) = self.index_stats.lock().unwrap().clone() {
                        let needs_admin = stats.roots.iter().any(|r| {
                            r.note
                                .as_deref()
                                .is_some_and(|n| n.contains("code=5"))
                        });
                        if needs_admin {
                            self.admin_prompt_once = true;
                            self.admin_prompt_open = true;
                        }
                    }
                } else {
                    self.admin_prompt_once = true;
                }
            }

            if self.admin_prompt_open {
                egui::Window::new("需要管理员权限")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .show(ctx, |ui| {
                        ui.label("检测到 USN/MFT 枚举被拒绝访问（code=5），已回退为 WalkDir 全盘扫描，因此索引会很慢。");
                        ui.label("是否现在以管理员身份重启以启用快速索引？");
                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("以管理员身份重启").clicked() {
                                if crate::windows_usn::relaunch_as_admin().is_ok() {
                                    std::process::exit(0);
                                }
                            }
                            if ui.button("继续（慢）").clicked() {
                                self.admin_prompt_open = false;
                            }
                        });
                    });
            }
        }
    }
}

impl FileSearchApp {
    fn show_search_tab(&mut self, ui: &mut egui::Ui) {
        // 搜索框
        ui.horizontal(|ui| {
            ui.label("搜索:");
            let response = ui.text_edit_singleline(&mut self.search_text);

            // 回车搜索
            if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                self.perform_search();
            }

            if ui.button("搜索").clicked() {
                self.perform_search();
            }
        });

        ui.separator();

        // 搜索选项（仅影响下一次“搜索”按钮/回车触发的搜索）
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.searcher.options.case_sensitive, "区分大小写");
            ui.checkbox(&mut self.searcher.options.path_search, "搜索路径");
            ui.checkbox(&mut self.searcher.options.fuzzy, "宽松搜索");
            ui.checkbox(&mut self.show_hidden, "显示隐藏文件");
        });

        // 文件类型过滤
        //（仅影响下一次“搜索”按钮/回车触发的搜索）
        ui.horizontal(|ui| {
            ui.label("文件类型:");
            egui::ComboBox::from_id_salt("file_type_filter")
                .selected_text(match self.file_type_filter {
                    FileTypeFilter::All => "全部",
                    FileTypeFilter::Files => "仅文件",
                    FileTypeFilter::Folders => "仅文件夹",
                    FileTypeFilter::Documents => "文档",
                    FileTypeFilter::Images => "图片",
                    FileTypeFilter::Videos => "视频",
                    FileTypeFilter::Audio => "音频",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.file_type_filter, FileTypeFilter::All, "全部");
                    ui.selectable_value(&mut self.file_type_filter, FileTypeFilter::Files, "仅文件");
                    ui.selectable_value(&mut self.file_type_filter, FileTypeFilter::Folders, "仅文件夹");
                    ui.selectable_value(&mut self.file_type_filter, FileTypeFilter::Documents, "文档 (doc/txt/pdf)");
                    ui.selectable_value(&mut self.file_type_filter, FileTypeFilter::Images, "图片 (jpg/png/gif)");
                    ui.selectable_value(&mut self.file_type_filter, FileTypeFilter::Videos, "视频 (mp4/avi/mkv)");
                    ui.selectable_value(&mut self.file_type_filter, FileTypeFilter::Audio, "音频 (mp3/wav/flac)");
                });

            ui.label(".ext");
            ui.text_edit_singleline(&mut self.file_extension);
        });

        ui.separator();

        // 结果列表
        let num_results = {
            let results = self.results.lock().unwrap();
            if self.show_hidden {
                results.len()
            } else {
                results.iter().filter(|r| !r.entry.is_hidden).count()
            }
        };

        ui.horizontal(|ui| {
            ui.label(format!("找到 {} 个结果", num_results));
            ui.label(egui::RichText::new("双击打开").small().weak());
        });

        // 使用 ScrollArea 显示结果
        egui::ScrollArea::vertical()
            .auto_shrink(false)
            .show(ui, |ui| {
                let results = self.results.lock().unwrap();
                for (idx, result) in results.iter().enumerate() {
                    let entry = &result.entry;
                    if !self.show_hidden && entry.is_hidden {
                        continue;
                    }

                    let is_selected = self.selected_result == Some(idx);

                    let row = ui
                        .horizontal(|ui| {
                            if entry.is_dir {
                                ui.label("📁");
                            } else {
                                ui.label("📄");
                            }

                            let name_color = if result.match_type == MatchType::Path {
                                egui::Color32::from_rgb(100, 100, 100)
                            } else {
                                egui::Color32::from_rgb(0, 0, 0)
                            };

                            ui.label(egui::RichText::new(&entry.name).color(name_color));

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(
                                    egui::RichText::new(Self::format_size(entry.size))
                                        .small()
                                        .weak(),
                                );
                            });
                        });

                    let response = ui.interact(row.response.rect, ui.id().with(idx), egui::Sense::click());

                    if is_selected {
                        ui.painter().rect_filled(
                            response.rect,
                            2.0,
                            egui::Color32::from_rgb(173, 216, 230),
                        );
                    }

                    // 悬停效果
                    if response.hovered() {
                        ui.painter().rect_filled(
                            response.rect,
                            2.0,
                            egui::Color32::from_rgb(220, 220, 220),
                        );
                    }

                    if response.clicked() {
                        self.selected_result = Some(idx);
                    }

                    if response.double_clicked() {
                        Self::open_path_in_os(&result.display_path);
                    }

                    // 路径提示
                    response.on_hover_text(&result.display_path);
                }
            });

        // 状态栏
        ui.separator();
        ui.horizontal(|ui| {
            if let Some(idx) = self.selected_result {
                let results = self.results.lock().unwrap();
                if let Some(result) = results.get(idx) {
                    let resp = ui
                        .add(
                            egui::Label::new(format!("选中: {}", result.display_path))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text("双击打开");
                    if resp.double_clicked() {
                        Self::open_path_in_os(&result.display_path);
                    }
                }
            }
        });
    }

    fn show_settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("索引设置");

        ui.label("索引来源: Windows (NTFS) 通过 USN/MFT 枚举（无本地索引缓存）");
        if let Some(t) = self.last_index_time {
            if let Ok(age) = SystemTime::now().duration_since(t) {
                ui.label(format!("上次开始索引: {} 秒前", age.as_secs()));
            }
        }
        #[cfg(windows)]
        {
            let elevated = match self.is_elevated {
                Some(v) => v,
                None => {
                    let v = crate::windows_usn::is_process_elevated().unwrap_or(false);
                    self.is_elevated = Some(v);
                    v
                }
            };
            ui.label(format!("管理员权限: {}", if elevated { "是" } else { "否" }));
            if !elevated {
                if ui.button("以管理员身份重启（启用快速索引）").clicked() {
                    if crate::windows_usn::relaunch_as_admin().is_ok() {
                        std::process::exit(0);
                    }
                }
            }
            ui.separator();
        }

        if let Some(stats) = self.index_stats.lock().unwrap().clone() {
            ui.label(format!("本次统计: 共 {} 项，用时 {} ms", stats.total_entries, stats.total_ms));
            ui.separator();
            ui.label("分路径统计（USN=快，WalkDir=慢/回退）：");
            for r in stats.roots {
                let src = match r.source {
                    IndexRootSource::Usn => "USN",
                    IndexRootSource::WalkDir => "WalkDir",
                };
                if let Some(note) = r.note {
                    ui.label(format!(
                        "- {src}: {} | {} 项 | {} ms | {}",
                        r.root, r.entries, r.duration_ms, note
                    ));
                } else {
                    ui.label(format!(
                        "- {src}: {} | {} 项 | {} ms",
                        r.root, r.entries, r.duration_ms
                    ));
                }
            }
        }

        ui.horizontal(|ui| {
            if ui.button("自动索引全部磁盘").clicked() {
                self.index_paths = Self::default_index_paths();
                self.rebuild_index();
            }
            if ui.button("重新索引").clicked() {
                self.rebuild_index();
            }
        });

        // 添加新路径
        ui.horizontal(|ui| {
            ui.label("添加路径:");
            let response = ui.text_edit_singleline(&mut self.new_path_input);
            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let path = PathBuf::from(&self.new_path_input);
                if path.exists() {
                    self.index_paths.push(path);
                    self.new_path_input.clear();
                    self.rebuild_index();
                }
            }
            if ui.button("添加").clicked() {
                let path = PathBuf::from(&self.new_path_input);
                if path.exists() {
                    self.index_paths.push(path);
                    self.new_path_input.clear();
                    self.rebuild_index();
                }
            }
        });

        // 收集要删除的索引
        let mut to_remove = Vec::new();
        for (idx, path) in self.index_paths.iter().enumerate() {
            ui.horizontal(|ui| {
                ui.label(path.to_string_lossy().as_ref());
                if ui.button("x").clicked() {
                    to_remove.push(idx);
                }
            });
        }

        // 执行删除
        for idx in to_remove.into_iter().rev() {
            self.index_paths.remove(idx);
        }

        ui.separator();

        if self.is_indexing {
            if self.index_progress.1 == 0 {
                ui.label(format!("索引中: 已扫描 {} 项", self.index_progress.0));
            } else {
                ui.label(format!(
                    "索引中: {} / {}",
                    self.index_progress.0, self.index_progress.1
                ));
            }
        }

        ui.separator();
        ui.heading("搜索设置");

        ui.checkbox(
            &mut self.searcher.options.case_sensitive,
            "默认区分大小写",
        );
        ui.checkbox(&mut self.searcher.options.path_search, "默认搜索路径");

        ui.separator();
        ui.heading("关于");
        ui.label(format!("文件搜索工具 v{}", env!("CARGO_PKG_VERSION")));
        ui.label(format!("作者：{}", env!("CARGO_PKG_AUTHORS")));
        ui.label("基于 Rust + egui 构建");
        ui.separator();
        ui.label("v0.1.2 更新内容：");
        ui.label(" - Windows: 基于 USN/MFT 枚举");
        ui.label(" - 启动后直接重建索引，不写入本地缓存文件");
        ui.label(" - 为加速启动，路径按需解析（展示/打开时再拼接）");
    }
}
