use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::UNIX_EPOCH;
use walkdir::WalkDir;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub name_lower: String,
    pub path: String,
    pub path_lower: String,
    pub size: u64,
    pub modified_ms: u64,
    pub is_dir: bool,
    pub is_hidden: bool,
}

pub struct FileIndexer {
    entries: Arc<Vec<FileEntry>>,
    name_index: HashMap<String, Vec<usize>>,
    total_files: Arc<AtomicUsize>,
    is_indexing: Arc<AtomicBool>,
    progress: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub struct IndexerHandles {
    pub total_files: Arc<AtomicUsize>,
    pub is_indexing: Arc<AtomicBool>,
    pub progress: Arc<AtomicUsize>,
}

#[derive(Serialize, Deserialize)]
struct IndexCache {
    version: u32,
    entries: Vec<FileEntry>,
}

#[derive(Serialize)]
struct IndexCacheRef<'a> {
    version: u32,
    entries: &'a [FileEntry],
}

impl FileIndexer {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(Vec::new()),
            name_index: HashMap::new(),
            total_files: Arc::new(AtomicUsize::new(0)),
            is_indexing: Arc::new(AtomicBool::new(false)),
            progress: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn is_indexing(&self) -> bool {
        self.is_indexing.load(Ordering::SeqCst)
    }

    pub fn progress(&self) -> (usize, usize) {
        (
            self.progress.load(Ordering::SeqCst),
            self.total_files.load(Ordering::SeqCst),
        )
    }

    pub fn get_entries(&self) -> &Vec<FileEntry> {
        &self.entries
    }

    pub fn handles(&self) -> IndexerHandles {
        IndexerHandles {
            total_files: Arc::clone(&self.total_files),
            is_indexing: Arc::clone(&self.is_indexing),
            progress: Arc::clone(&self.progress),
        }
    }

    pub fn begin_indexing(&self) {
        self.is_indexing.store(true, Ordering::SeqCst);
        self.progress.store(0, Ordering::SeqCst);
        self.total_files.store(0, Ordering::SeqCst);
    }

    pub fn replace_index(
        &mut self,
        all_entries: Vec<FileEntry>,
        name_index: HashMap<String, Vec<usize>>,
    ) {
        let count = all_entries.len();
        self.entries = Arc::new(all_entries);
        self.name_index = name_index;
        self.total_files.store(count, Ordering::SeqCst);
        self.progress.store(count, Ordering::SeqCst);
        self.is_indexing.store(false, Ordering::SeqCst);
    }

    pub fn set_entries_from_cache(&mut self, entries: Vec<FileEntry>) {
        let mut name_index: HashMap<String, Vec<usize>> = HashMap::new();
        for (idx, entry) in entries.iter().enumerate() {
            name_index
                .entry(entry.name_lower.clone())
                .or_insert_with(Vec::new)
                .push(idx);
        }

        self.entries = Arc::new(entries);
        self.name_index = name_index;
        self.total_files.store(self.entries.len(), Ordering::SeqCst);
        self.progress.store(self.entries.len(), Ordering::SeqCst);
        self.is_indexing.store(false, Ordering::SeqCst);
    }

    pub fn build_index_snapshot(
        root_paths: Vec<PathBuf>,
        handles: Option<&IndexerHandles>,
    ) -> (Vec<FileEntry>, HashMap<String, Vec<usize>>) {
        let mut all_entries: Vec<FileEntry> = Vec::new();
        let mut name_index: HashMap<String, Vec<usize>> = HashMap::new();
        let mut count: usize = 0;

        for root_path in &root_paths {
            if !root_path.exists() {
                continue;
            }

            for entry in WalkDir::new(root_path)
                .follow_links(false)
                .same_file_system(true)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if let Some(handles) = handles {
                    if !handles.is_indexing.load(Ordering::SeqCst) {
                        return (all_entries, name_index);
                    }
                }

                let path = entry.path();
                let metadata = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                let is_dir = metadata.is_dir();
                let is_hidden = is_path_hidden(path, &metadata);

                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();

                let path_str = path.to_string_lossy().replace("\\", "/");
                let name_lower = name.to_lowercase();
                let path_lower = path_str.to_lowercase();

                let modified_ms = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);

                let file_entry = FileEntry {
                    name: name.clone(),
                    name_lower: name_lower.clone(),
                    path: path_str,
                    path_lower,
                    size: metadata.len(),
                    modified_ms,
                    is_dir,
                    is_hidden,
                };

                let idx = all_entries.len();
                all_entries.push(file_entry);

                name_index
                    .entry(name_lower)
                    .or_insert_with(Vec::new)
                    .push(idx);

                count += 1;
                if count % 1000 == 0 {
                    if let Some(handles) = handles {
                        handles.progress.store(count, Ordering::SeqCst);
                    }
                }
            }
        }

        if let Some(handles) = handles {
            handles.progress.store(count, Ordering::SeqCst);
        }

        (all_entries, name_index)
    }

    pub fn load_cache(cache_path: &Path) -> std::io::Result<Vec<FileEntry>> {
        let bytes = std::fs::read(cache_path)?;
        let cache: IndexCache = bincode::deserialize(&bytes).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("反序列化失败: {e}"))
        })?;
        if cache.version != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "缓存版本不匹配",
            ));
        }
        Ok(cache.entries)
    }

    pub fn save_cache(cache_path: &Path, entries: &[FileEntry]) -> std::io::Result<()> {
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let cache = IndexCacheRef {
            version: 1,
            entries,
        };
        let bytes = bincode::serialize(&cache).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("序列化失败: {e}"))
        })?;

        let tmp_path = cache_path.with_extension("tmp");
        std::fs::write(&tmp_path, bytes)?;
        let _ = std::fs::remove_file(cache_path);
        std::fs::rename(tmp_path, cache_path)?;
        Ok(())
    }

    pub fn search(&self, pattern: &str, case_sensitive: bool, max_results: usize) -> Vec<&FileEntry> {
        if pattern.is_empty() {
            return Vec::new();
        }

        let pattern = if case_sensitive {
            pattern.to_string()
        } else {
            pattern.to_lowercase()
        };

        let mut results: Vec<&FileEntry> = Vec::with_capacity(max_results);

        // 使用索引加速搜索
        for (key, indices) in &self.name_index {
            let key_matches = if case_sensitive {
                key.contains(&pattern)
            } else {
                key.to_lowercase().contains(&pattern)
            };

            if key_matches {
                for &idx in indices {
                    if let Some(entry) = self.entries.get(idx) {
                        results.push(entry);
                        if results.len() >= max_results {
                            return results;
                        }
                    }
                }
            }
        }

        results
    }

    pub fn start_indexing(&mut self, root_paths: Vec<PathBuf>) {
        self.is_indexing.store(true, Ordering::SeqCst);
        self.progress.store(0, Ordering::SeqCst);
        self.total_files.store(0, Ordering::SeqCst);

        let _entries = Arc::clone(&self.entries);
        let total_files = Arc::clone(&self.total_files);
        let progress = Arc::clone(&self.progress);
        let is_indexing = Arc::clone(&self.is_indexing);

        thread::spawn(move || {
            let mut _all_entries: Vec<FileEntry> = Vec::new();
            let mut _name_index: HashMap<String, Vec<usize>> = HashMap::new();
            let mut count = 0;

            for root_path in &root_paths {
                if !root_path.exists() {
                    continue;
                }

                for entry in WalkDir::new(root_path)
                    .follow_links(false)
                    .same_file_system(true)
                    .into_iter()
                    .filter_map(|e| e.ok())
                {
                    if !is_indexing.load(Ordering::SeqCst) {
                        return;
                    }

                    let path = entry.path();
                    let metadata = match entry.metadata() {
                        Ok(m) => m,
                        Err(_) => continue,
                    };

                    let is_dir = metadata.is_dir();
                    let is_hidden = is_path_hidden(path, &metadata);

                    let name = path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();

                    let path_str = path.to_string_lossy().replace("\\", "/");
                    let name_lower = name.to_lowercase();
                    let path_lower = path_str.to_lowercase();

                    let modified_ms = metadata
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);

                    let file_entry = FileEntry {
                        name: name.clone(),
                        name_lower: name_lower.clone(),
                        path: path_str,
                        path_lower,
                        size: metadata.len(),
                        modified_ms,
                        is_dir,
                        is_hidden,
                    };

                    let idx = _all_entries.len();
                    _all_entries.push(file_entry);

                    // 建立名称索引
                    _name_index
                        .entry(name_lower)
                        .or_insert_with(Vec::new)
                        .push(idx);

                    count += 1;
                    if count % 1000 == 0 {
                        progress.store(count, Ordering::SeqCst);
                    }
                }
            }

            total_files.store(count, Ordering::SeqCst);
            progress.store(count, Ordering::SeqCst);

            // 更新共享状态
            // 注意: 这里需要通知主线程更新
        });

        // 等待索引完成
    }

    pub fn build_index(&mut self, root_paths: Vec<PathBuf>) {
        self.is_indexing.store(true, Ordering::SeqCst);
        self.progress.store(0, Ordering::SeqCst);
        self.total_files.store(0, Ordering::SeqCst);

        let mut all_entries: Vec<FileEntry> = Vec::new();
        let mut name_index: HashMap<String, Vec<usize>> = HashMap::new();
        let mut count = 0;

        for root_path in &root_paths {
            if !root_path.exists() {
                continue;
            }

            for entry in WalkDir::new(root_path)
                .follow_links(false)
                .same_file_system(true)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                let path = entry.path();
                let metadata = match entry.metadata() {
                    Ok(m) => m,
                    Err(_) => continue,
                };

                let is_dir = metadata.is_dir();
                let is_hidden = is_path_hidden(path, &metadata);

                let name = path.file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();

                let path_str = path.to_string_lossy().replace("\\", "/");
                let name_lower = name.to_lowercase();
                let path_lower = path_str.to_lowercase();

                let modified_ms = metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);

                let file_entry = FileEntry {
                    name: name.clone(),
                    name_lower: name_lower.clone(),
                    path: path_str,
                    path_lower,
                    size: metadata.len(),
                    modified_ms,
                    is_dir,
                    is_hidden,
                };

                let idx = all_entries.len();
                all_entries.push(file_entry);

                // 建立名称索引
                name_index
                    .entry(name_lower)
                    .or_insert_with(Vec::new)
                    .push(idx);

                count += 1;
                if count % 1000 == 0 {
                    self.progress.store(count, Ordering::SeqCst);
                }
            }
        }

        self.total_files.store(count, Ordering::SeqCst);
        self.progress.store(count, Ordering::SeqCst);

        self.entries = Arc::new(all_entries);
        self.name_index = name_index;
        self.is_indexing.store(false, Ordering::SeqCst);
    }

    pub fn stop(&self) {
        self.is_indexing.store(false, Ordering::SeqCst);
    }
}

impl Default for FileIndexer {
    fn default() -> Self {
        Self::new()
    }
}

fn is_path_hidden(path: &Path, metadata: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;
        let attrs = metadata.file_attributes();
        (attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM)) != 0
    }

    #[cfg(not(windows))]
    {
        match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name.starts_with('.'),
            None => false,
        }
    }
}
