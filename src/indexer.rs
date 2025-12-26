use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use walkdir::WalkDir;

use bincode::Options;
use serde::{Deserialize, Serialize};

const CACHE_MAGIC: [u8; 4] = *b"RSIX";
const CACHE_HEADER_LEN: usize = 8;
const CACHE_V2: u8 = 2;
const CACHE_ENCODING_VARINT: u8 = 1;

#[derive(Clone, Debug)]
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
struct IndexCacheV1 {
    version: u32,
    entries: Vec<FileEntryV1>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileEntryV1 {
    name: String,
    name_lower: String,
    path: String,
    path_lower: String,
    size: u64,
    modified_ms: u64,
    is_dir: bool,
    is_hidden: bool,
}

#[derive(Serialize, Deserialize)]
struct IndexCachePayloadV2 {
    entries: Vec<DiskEntryV2>,
}

#[derive(Serialize, Deserialize)]
struct DiskEntryV2 {
    path: String,
    size: u64,
    modified_ms: u64,
    flags: u8,
}

#[derive(Serialize)]
struct IndexCachePayloadV2Ref<'a> {
    entries: Vec<DiskEntryV2Ref<'a>>,
}

#[derive(Serialize)]
struct DiskEntryV2Ref<'a> {
    path: &'a str,
    size: u64,
    modified_ms: u64,
    flags: u8,
}

impl<'a> DiskEntryV2Ref<'a> {
    fn from_entry(entry: &'a FileEntry) -> Self {
        let mut flags = 0u8;
        if entry.is_dir {
            flags |= 1 << 0;
        }
        if entry.is_hidden {
            flags |= 1 << 1;
        }
        Self {
            path: entry.path.as_str(),
            size: entry.size,
            modified_ms: entry.modified_ms,
            flags,
        }
    }
}

impl DiskEntryV2 {
    fn from_entry(entry: &FileEntry) -> Self {
        let mut flags = 0u8;
        if entry.is_dir {
            flags |= 1 << 0;
        }
        if entry.is_hidden {
            flags |= 1 << 1;
        }
        Self {
            path: entry.path.clone(),
            size: entry.size,
            modified_ms: entry.modified_ms,
            flags,
        }
    }

    fn to_entry(&self) -> FileEntry {
        let name = file_name_from_normalized_path(&self.path);
        let name_lower = lowercase_for_search(&name);
        let path_lower = lowercase_for_search(&self.path);
        FileEntry {
            name,
            name_lower,
            path: self.path.clone(),
            path_lower,
            size: self.size,
            modified_ms: self.modified_ms,
            is_dir: (self.flags & (1 << 0)) != 0,
            is_hidden: (self.flags & (1 << 1)) != 0,
        }
    }
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

    pub fn entries_arc(&self) -> Arc<Vec<FileEntry>> {
        Arc::clone(&self.entries)
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
        self.entries = Arc::new(entries);
        // 当前 UI 搜索走 `Searcher` 全量扫描，不依赖 `name_index`；
        // 这里避免构建 HashMap 以加速启动/加载缓存。
        self.name_index = HashMap::new();
        self.total_files.store(self.entries.len(), Ordering::SeqCst);
        self.progress.store(self.entries.len(), Ordering::SeqCst);
        self.is_indexing.store(false, Ordering::SeqCst);
    }

    pub fn build_index_snapshot(
        root_paths: Vec<PathBuf>,
        handles: Option<&IndexerHandles>,
    ) -> (Vec<FileEntry>, HashMap<String, Vec<usize>>) {
        let mut all_entries: Vec<FileEntry> = Vec::new();
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
                        return (all_entries, HashMap::new());
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
                let name_lower = lowercase_for_search(&name);
                let path_lower = lowercase_for_search(&path_str);

                let file_entry = FileEntry {
                    name,
                    name_lower,
                    path: path_str,
                    path_lower,
                    size: metadata.len(),
                    modified_ms: 0,
                    is_dir,
                    is_hidden,
                };

                all_entries.push(file_entry);

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

        (all_entries, HashMap::new())
    }

    pub fn load_cache(cache_path: &Path) -> std::io::Result<Vec<FileEntry>> {
        let bytes = std::fs::read(cache_path)?;
        if bytes.len() >= CACHE_HEADER_LEN && bytes.starts_with(&CACHE_MAGIC) {
            return load_cache_v2(&bytes);
        }

        // 兼容旧缓存（v1：纯 bincode + 包含 name_lower/path_lower）
        let cache: IndexCacheV1 = bincode::deserialize(&bytes).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("反序列化失败: {e}"))
        })?;
        if cache.version != 1 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "缓存版本不匹配"));
        }
        let entries: Vec<FileEntry> = cache
            .entries
            .into_iter()
            .map(|e| FileEntry {
                name: e.name,
                name_lower: e.name_lower,
                path: e.path,
                path_lower: e.path_lower,
                size: e.size,
                modified_ms: e.modified_ms,
                is_dir: e.is_dir,
                is_hidden: e.is_hidden,
            })
            .collect();

        // 尝试自动升级到更小的 v2 缓存格式（失败则忽略，避免影响启动）
        let _ = Self::save_cache(cache_path, &entries);

        Ok(entries)
    }

    pub fn save_cache(cache_path: &Path, entries: &[FileEntry]) -> std::io::Result<()> {
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        // v2：写入更小的磁盘格式（去掉 name_lower/path_lower 等重复字段）
        // 文件格式：RSIX(4) + version(u8) + encoding(u8) + reserved(u16) + bincode(payload)
        // 这里用借用版 payload，避免对每个 entry 的 path 做 clone（会显著拖慢大索引的缓存写入）。
        let payload = IndexCachePayloadV2Ref {
            entries: entries.iter().map(DiskEntryV2Ref::from_entry).collect(),
        };
        let options = bincode::DefaultOptions::new().with_varint_encoding();
        let payload_bytes = options.serialize(&payload).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("序列化失败: {e}"))
        })?;
        let mut bytes = Vec::with_capacity(CACHE_HEADER_LEN + payload_bytes.len());
        bytes.extend_from_slice(&CACHE_MAGIC);
        bytes.push(CACHE_V2);
        bytes.push(CACHE_ENCODING_VARINT);
        bytes.extend_from_slice(&[0, 0]);
        bytes.extend_from_slice(&payload_bytes);

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
            lowercase_for_search(pattern)
        };

        let mut results: Vec<&FileEntry> = Vec::with_capacity(max_results);

        // 兼容：如果 `name_index` 没有构建（默认），则直接扫描 entries。
        if self.name_index.is_empty() {
            for entry in self.entries.iter() {
                let haystack = if case_sensitive {
                    entry.name.as_str()
                } else {
                    entry.name_lower.as_str()
                };
                if haystack.contains(&pattern) {
                    results.push(entry);
                    if results.len() >= max_results {
                        return results;
                    }
                }
            }
            return results;
        }

        // 使用索引加速搜索（保留旧逻辑：如果某处仍然构建了 name_index）
        for (key, indices) in &self.name_index {
            let key_matches = if case_sensitive {
                // key 可能是 lower，也可能是原始（取决于构建方式），保守处理
                key.contains(&pattern)
            } else {
                key.contains(&pattern)
            };

            if !key_matches {
                continue;
            }
            for &idx in indices {
                if let Some(entry) = self.entries.get(idx) {
                    results.push(entry);
                    if results.len() >= max_results {
                        return results;
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
                    let name_lower = lowercase_for_search(&name);
                    let path_lower = lowercase_for_search(&path_str);

                    let file_entry = FileEntry {
                        name,
                        name_lower,
                        path: path_str,
                        path_lower,
                        size: metadata.len(),
                        modified_ms: 0,
                        is_dir,
                        is_hidden,
                    };

                    _all_entries.push(file_entry);

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
                let name_lower = lowercase_for_search(&name);
                let path_lower = lowercase_for_search(&path_str);

                let file_entry = FileEntry {
                    name,
                    name_lower,
                    path: path_str,
                    path_lower,
                    size: metadata.len(),
                    modified_ms: 0,
                    is_dir,
                    is_hidden,
                };

                all_entries.push(file_entry);

                count += 1;
                if count % 1000 == 0 {
                    self.progress.store(count, Ordering::SeqCst);
                }
            }
        }

        self.total_files.store(count, Ordering::SeqCst);
        self.progress.store(count, Ordering::SeqCst);

        self.entries = Arc::new(all_entries);
        self.name_index = HashMap::new();
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

fn load_cache_v2(bytes: &[u8]) -> std::io::Result<Vec<FileEntry>> {
    if bytes.len() < CACHE_HEADER_LEN {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "缓存文件过短",
        ));
    }
    if !bytes.starts_with(&CACHE_MAGIC) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "缓存魔数不匹配",
        ));
    }

    let version = bytes[4];
    let encoding = bytes[5];
    if version != CACHE_V2 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "缓存版本不匹配",
        ));
    }
    if encoding != CACHE_ENCODING_VARINT {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "缓存编码不支持",
        ));
    }

    let payload_bytes = &bytes[CACHE_HEADER_LEN..];
    let options = bincode::DefaultOptions::new().with_varint_encoding();
    let payload: IndexCachePayloadV2 = options.deserialize(payload_bytes).map_err(|e| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("反序列化失败: {e}"))
    })?;
    Ok(payload.entries.into_iter().map(|e| e.to_entry()).collect())
}

fn file_name_from_normalized_path(path: &str) -> String {
    if path.ends_with('/') {
        return String::new();
    }
    let mut it = path.rsplit('/');
    match it.next() {
        Some("") => it.next().unwrap_or("").to_string(),
        Some(name) => name.to_string(),
        None => String::new(),
    }
}

fn lowercase_for_search(s: &str) -> String {
    if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        s.to_lowercase()
    }
}
