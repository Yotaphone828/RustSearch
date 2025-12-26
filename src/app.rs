use eframe::egui;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime};

use crate::indexer::FileIndexer;
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
    cache_loaded: bool,
    cache_mtime: Option<SystemTime>,
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
            cache_loaded: false,
            cache_mtime: None,
        }
    }
}

impl FileSearchApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let mut app = Self::default();

        let cache_path = Self::index_cache_path();
        let loaded = app.try_load_index_cache(&cache_path);
        let stale = loaded && Self::cache_is_stale(&cache_path);
        if !loaded || stale {
            app.rebuild_index();
        }

        app
    }

    fn index_cache_path() -> PathBuf {
        if cfg!(windows) {
            if let Some(base) = std::env::var_os("LOCALAPPDATA") {
                return PathBuf::from(base).join("world_hello").join("index_cache.bin");
            }
            if let Some(base) = std::env::var_os("APPDATA") {
                return PathBuf::from(base).join("world_hello").join("index_cache.bin");
            }
        }

        PathBuf::from(".").join("index_cache.bin")
    }

    fn cache_is_stale(cache_path: &PathBuf) -> bool {
        let Ok(meta) = std::fs::metadata(cache_path) else {
            return true;
        };
        let Ok(mtime) = meta.modified() else {
            return true;
        };
        let Ok(age) = SystemTime::now().duration_since(mtime) else {
            return true;
        };
        age > Duration::from_secs(24 * 60 * 60)
    }

    fn try_load_index_cache(&mut self, cache_path: &PathBuf) -> bool {
        if !cache_path.is_file() {
            return false;
        }
        let Ok((entries, usn_states)) = FileIndexer::load_cache(cache_path) else {
            return false;
        };

        let mut indexer_guard = self.indexer.lock().unwrap();
        indexer_guard.set_cache(entries, usn_states);
        self.cache_loaded = true;
        self.cache_mtime = std::fs::metadata(cache_path).ok().and_then(|m| m.modified().ok());
        true
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
        let cache_path = Self::index_cache_path();
        let index_seq = Arc::clone(&self.index_seq);
        let seq = index_seq.fetch_add(1, Ordering::SeqCst) + 1;

        let handles = {
            let indexer_guard = indexer.lock().unwrap();
            indexer_guard.begin_indexing();
            indexer_guard.handles()
        };

        thread::spawn(move || {
            // 优先尝试 USN 增量更新（Windows + 已有 USN 状态），失败再全量重建
            #[cfg(windows)]
            {
                if index_seq.load(Ordering::SeqCst) != seq {
                    return;
                }

                // 只有在“索引目标是盘符根目录集合”且与缓存内 USN 状态一致时，才做增量更新；
                // 否则（比如新增了某个子目录路径）必须走全量重建，才能把范围变更纳入索引。
                let mut requested_drives: Vec<u8> = paths
                    .iter()
                    .filter_map(|p| crate::windows_usn::is_drive_root(p).map(|d| d as u8))
                    .collect();
                requested_drives.sort_unstable();
                requested_drives.dedup();

                let mut indexer_guard = indexer.lock().unwrap();
                let mut cached_drives: Vec<u8> =
                    indexer_guard.usn_states().iter().map(|s| s.drive).collect();
                cached_drives.sort_unstable();
                cached_drives.dedup();

                if !requested_drives.is_empty() && requested_drives == cached_drives {
                    match indexer_guard.try_apply_usn_incremental(&handles) {
                        Ok(true) => {
                            drop(indexer_guard);
                            if index_seq.load(Ordering::SeqCst) != seq {
                                return;
                            }
                            let (entries_arc, usn_states) = {
                                let indexer_guard = indexer.lock().unwrap();
                                (indexer_guard.entries_arc(), indexer_guard.usn_states_clone())
                            };
                            let _ = FileIndexer::save_cache(
                                &cache_path,
                                entries_arc.as_slice(),
                                &usn_states,
                            );
                            return;
                        }
                        Ok(false) => {}
                        Err(_) => {}
                    }
                }
            }

            let (entries, usn_states) = FileIndexer::build_index_snapshot(paths, Some(&handles));
            if index_seq.load(Ordering::SeqCst) != seq {
                return;
            }
            {
                // 先更新内存索引，让搜索尽快可用；缓存写入放到后面，不阻塞“索引完成”的体验
                let mut indexer_guard = indexer.lock().unwrap();
                indexer_guard.replace_index(entries, usn_states);
            }

            if index_seq.load(Ordering::SeqCst) != seq {
                return;
            }

            let (entries_arc, usn_states) = {
                let indexer_guard = indexer.lock().unwrap();
                (indexer_guard.entries_arc(), indexer_guard.usn_states_clone())
            };
            let _ = FileIndexer::save_cache(&cache_path, entries_arc.as_slice(), &usn_states);
        });
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
                        Self::open_path_in_os(&entry.path);
                    }

                    // 路径提示
                    response.on_hover_text(&entry.path);
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
                            egui::Label::new(format!("选中: {}", result.entry.path))
                                .sense(egui::Sense::click()),
                        )
                        .on_hover_text("双击打开");
                    if resp.double_clicked() {
                        Self::open_path_in_os(&result.entry.path);
                    }
                }
            }
        });
    }

    fn show_settings_tab(&mut self, ui: &mut egui::Ui) {
        ui.heading("索引设置");

        ui.label(format!("缓存文件: {}", Self::index_cache_path().display()));
        if self.cache_loaded {
            ui.label("缓存: 已加载（用于加速启动）");
            if let Some(t) = self.cache_mtime {
                if let Ok(age) = SystemTime::now().duration_since(t) {
                    ui.label(format!("缓存时间: {} 分钟前", age.as_secs() / 60));
                }
            }
        } else {
            ui.label("缓存: 未加载（首次运行会较慢）");
        }
        ui.label("缓存自动过期: 24 小时");

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
        ui.label("v0.1.1 更新内容：");
        ui.label(" - 优化索引缓存机制（缓存更小，旧缓存自动升级）");
        ui.label(" - Windows: 使用 USN Journal 加速全盘枚举（NTFS）");
        ui.label(" - Windows: 使用 USN Journal 增量更新（避免频繁全盘扫描）");
    }
}
