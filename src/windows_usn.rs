#![cfg(windows)]

use crate::indexer::{FileEntry, IndexerHandles, UsnDriveState};
use std::collections::HashMap;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use winapi::shared::minwindef::{BOOL, DWORD, LPVOID};
use winapi::shared::ntdef::HANDLE;
use winapi::um::fileapi::{CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::ioapiset::DeviceIoControl;
use winapi::um::winnt::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, GENERIC_READ,
};

const OPEN_EXISTING: DWORD = 3;
const FILE_FLAG_BACKUP_SEMANTICS: DWORD = 0x0200_0000;

// 来自 winioctl.h 的常量值（避免依赖 winapi 的 winioctl feature/符号差异）
const FSCTL_QUERY_USN_JOURNAL: DWORD = 0x0009_00F4;
const FSCTL_ENUM_USN_DATA: DWORD = 0x0009_00B3;
const FSCTL_READ_USN_JOURNAL: DWORD = 0x0009_00BB;

const USN_REASON_FILE_CREATE: DWORD = 0x0000_0100;
const USN_REASON_FILE_DELETE: DWORD = 0x0000_0200;
const USN_REASON_RENAME_OLD_NAME: DWORD = 0x0000_1000;
const USN_REASON_RENAME_NEW_NAME: DWORD = 0x0000_2000;

#[repr(C)]
struct USN_JOURNAL_DATA_V0 {
    usn_journal_id: u64,
    _first_usn: i64,
    next_usn: i64,
    _lowest_valid_usn: i64,
    _max_usn: i64,
    _max_size: u64,
    _allocation_delta: u64,
}

#[repr(C)]
struct MFT_ENUM_DATA_V0 {
    start_file_reference_number: u64,
    low_usn: i64,
    high_usn: i64,
}

#[repr(C)]
struct USN_RECORD_V2 {
    record_length: DWORD,
    major_version: u16,
    minor_version: u16,
    file_reference_number: u64,
    parent_file_reference_number: u64,
    _usn: i64,
    _time_stamp: i64,
    _reason: DWORD,
    _source_info: DWORD,
    _security_id: DWORD,
    file_attributes: DWORD,
    file_name_length: u16,
    file_name_offset: u16,
    // 后面跟变长文件名（UTF-16）
}

#[repr(C)]
struct READ_USN_JOURNAL_DATA_V0 {
    start_usn: i64,
    reason_mask: DWORD,
    return_only_on_close: DWORD,
    timeout: u64,
    bytes_to_wait_for: u64,
    usn_journal_id: u64,
}

struct Node {
    parent: u64,
    name: String,
    attrs: DWORD,
}

struct UsnEvent {
    frn: u64,
    parent_frn: u64,
    attrs: DWORD,
    reason: DWORD,
    name: String,
}

pub fn try_apply_usn_incremental(
    entries: &mut Vec<FileEntry>,
    usn_states: &mut Vec<UsnDriveState>,
    handles: &IndexerHandles,
) -> io::Result<()> {
    if usn_states.is_empty() {
        return Ok(());
    }

    for state in usn_states.iter_mut() {
        if !handles.is_indexing.load(Ordering::SeqCst) {
            return Ok(());
        }
        let drive = state.drive as char;
        let events = read_usn_events(drive, state, &*handles.is_indexing, &*handles.progress)?;
        apply_events_for_drive(entries, state, events);
    }

    Ok(())
}

pub fn is_drive_root(path: &Path) -> Option<char> {
    let s = path.to_string_lossy();
    let mut chars = s.chars();
    let drive = chars.next()?;
    if !drive.is_ascii_alphabetic() {
        return None;
    }
    if chars.next()? != ':' {
        return None;
    }
    let rest: String = chars.collect();
    if rest.is_empty() || rest.chars().all(|c| c == '\\' || c == '/') {
        Some(drive.to_ascii_uppercase())
    } else {
        None
    }
}

pub fn try_enumerate_drive_root(
    root_path: &Path,
    progress_base: usize,
    is_indexing: Option<&AtomicBool>,
    progress: Option<&AtomicUsize>,
) -> io::Result<(Vec<FileEntry>, UsnDriveState)> {
    let Some(drive) = is_drive_root(root_path) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "不是盘符根目录"));
    };

    let volume_handle = open_volume_handle(drive)?;
    let root_frn = query_root_frn(drive)?;

    let journal = query_usn_journal(volume_handle)?;
    let mut enum_data = MFT_ENUM_DATA_V0 {
        start_file_reference_number: 0,
        low_usn: 0,
        high_usn: journal.next_usn,
    };

    // 1MB 缓冲区：在大盘上可减少 ioctl 次数
    let mut buffer = vec![0u8; 1024 * 1024];

    let mut nodes: HashMap<u64, Node> = HashMap::new();
    let mut seen = 0usize;

    loop {
        if let Some(flag) = is_indexing {
            if !flag.load(Ordering::SeqCst) {
                unsafe {
                    CloseHandle(volume_handle);
                }
                return Ok((
                    Vec::new(),
                    UsnDriveState {
                        drive: drive as u8,
                        journal_id: journal.usn_journal_id,
                        root_frn,
                        last_usn: journal.next_usn,
                    },
                ));
            }
        }

        let mut bytes_returned: DWORD = 0;
        let ok: BOOL = unsafe {
            DeviceIoControl(
                volume_handle,
                FSCTL_ENUM_USN_DATA,
                &mut enum_data as *mut _ as LPVOID,
                std::mem::size_of::<MFT_ENUM_DATA_V0>() as DWORD,
                buffer.as_mut_ptr() as LPVOID,
                buffer.len() as DWORD,
                &mut bytes_returned as *mut DWORD,
                ptr::null_mut(),
            )
        };

        if ok == 0 {
            let err = io::Error::last_os_error();
            // 枚举结束通常是 ERROR_HANDLE_EOF(38)
            if err.raw_os_error() == Some(38) {
                break;
            }
            unsafe {
                CloseHandle(volume_handle);
            }
            return Err(err);
        }

        if (bytes_returned as usize) <= std::mem::size_of::<u64>() {
            break;
        }

        // 输出缓冲区开头是 “下一个起始 FRN”
        let next_frn = unsafe { *(buffer.as_ptr() as *const u64) };
        enum_data.start_file_reference_number = next_frn;

        let mut offset = std::mem::size_of::<u64>();
        while offset + std::mem::size_of::<USN_RECORD_V2>() <= bytes_returned as usize {
            let record_ptr = unsafe { buffer.as_ptr().add(offset) as *const USN_RECORD_V2 };
            let record_len = unsafe { (*record_ptr).record_length as usize };
            if record_len == 0 || offset + record_len > bytes_returned as usize {
                break;
            }

            let major = unsafe { (*record_ptr).major_version };
            if major != 2 {
                offset += record_len;
                continue;
            }

            let frn = unsafe { (*record_ptr).file_reference_number };
            let parent = unsafe { (*record_ptr).parent_file_reference_number };
            let attrs = unsafe { (*record_ptr).file_attributes };
            let name_len_bytes = unsafe { (*record_ptr).file_name_length as usize };
            let name_off = unsafe { (*record_ptr).file_name_offset as usize };

            if name_len_bytes > 0 && name_off + name_len_bytes <= record_len {
                let name_ptr = unsafe { (record_ptr as *const u8).add(name_off) as *const u16 };
                let name_len_u16 = name_len_bytes / 2;
                let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len_u16) };
                let name = String::from_utf16_lossy(name_slice);
                if !name.is_empty() {
                    nodes.insert(
                        frn,
                        Node {
                            parent,
                            name,
                            attrs,
                        },
                    );
                    seen += 1;
                    if seen % 50_000 == 0 {
                        if let Some(p) = progress {
                            p.store(progress_base.saturating_add(seen), Ordering::SeqCst);
                        }
                    }
                }
            }

            offset += record_len;
        }
    }

    unsafe {
        CloseHandle(volume_handle);
    }

    let mut path_cache: HashMap<u64, String> = HashMap::new();
    path_cache.insert(root_frn, format!("{drive}:/"));

    let mut entries: Vec<FileEntry> = Vec::with_capacity(nodes.len());
    for (frn, node) in nodes.iter() {
        if *frn == root_frn {
            continue;
        }
        let Some(full_path) = build_full_path(*frn, root_frn, drive, &nodes, &mut path_cache) else {
            continue;
        };

        let name_lower = lowercase_for_search(&node.name);
        let path_lower = lowercase_for_search(&full_path);
        let is_dir = (node.attrs & FILE_ATTRIBUTE_DIRECTORY) != 0;
        let is_hidden = (node.attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM)) != 0;

        entries.push(FileEntry {
            name: node.name.clone(),
            name_lower,
            path: full_path,
            path_lower,
            drive: drive as u8,
            frn: *frn,
            parent_frn: node.parent,
            size: u64::MAX,
            modified_ms: 0,
            is_dir,
            is_hidden,
        });
    }

    if let Some(p) = progress {
        p.store(progress_base.saturating_add(entries.len()), Ordering::SeqCst);
    }

    Ok((
        entries,
        UsnDriveState {
            drive: drive as u8,
            journal_id: journal.usn_journal_id,
            root_frn,
            last_usn: journal.next_usn,
        },
    ))
}

fn read_usn_events(
    drive: char,
    state: &mut UsnDriveState,
    is_indexing: &AtomicBool,
    progress: &AtomicUsize,
) -> io::Result<Vec<UsnEvent>> {
    let volume_handle = open_volume_handle(drive)?;
    let journal = query_usn_journal(volume_handle)?;
    if journal.usn_journal_id != state.journal_id {
        unsafe {
            CloseHandle(volume_handle);
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "USN Journal 已变更，需要全量重建",
        ));
    }

    let mut input = READ_USN_JOURNAL_DATA_V0 {
        start_usn: state.last_usn,
        reason_mask: 0xFFFF_FFFF,
        return_only_on_close: 0,
        timeout: 0,
        bytes_to_wait_for: 0,
        usn_journal_id: state.journal_id,
    };

    let mut buffer = vec![0u8; 1024 * 1024];
    let mut events: Vec<UsnEvent> = Vec::new();

    loop {
        if !is_indexing.load(Ordering::SeqCst) {
            unsafe {
                CloseHandle(volume_handle);
            }
            return Ok(Vec::new());
        }

        let mut bytes_returned: DWORD = 0;
        let ok: BOOL = unsafe {
            DeviceIoControl(
                volume_handle,
                FSCTL_READ_USN_JOURNAL,
                &mut input as *mut _ as LPVOID,
                std::mem::size_of::<READ_USN_JOURNAL_DATA_V0>() as DWORD,
                buffer.as_mut_ptr() as LPVOID,
                buffer.len() as DWORD,
                &mut bytes_returned as *mut DWORD,
                ptr::null_mut(),
            )
        };

        if ok == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(38) {
                break;
            }
            unsafe {
                CloseHandle(volume_handle);
            }
            return Err(err);
        }

        if (bytes_returned as usize) < std::mem::size_of::<i64>() {
            break;
        }

        let next_usn = unsafe { *(buffer.as_ptr() as *const i64) };
        input.start_usn = next_usn;
        state.last_usn = next_usn;

        if bytes_returned as usize == std::mem::size_of::<i64>() {
            break;
        }

        let mut offset = std::mem::size_of::<i64>();
        while offset + std::mem::size_of::<USN_RECORD_V2>() <= bytes_returned as usize {
            let record_ptr = unsafe { buffer.as_ptr().add(offset) as *const USN_RECORD_V2 };
            let record_len = unsafe { (*record_ptr).record_length as usize };
            if record_len == 0 || offset + record_len > bytes_returned as usize {
                break;
            }

            let major = unsafe { (*record_ptr).major_version };
            if major != 2 {
                offset += record_len;
                continue;
            }

            let reason = unsafe { (*record_ptr)._reason };
            if (reason
                & (USN_REASON_FILE_CREATE
                    | USN_REASON_FILE_DELETE
                    | USN_REASON_RENAME_NEW_NAME
                    | USN_REASON_RENAME_OLD_NAME))
                == 0
            {
                offset += record_len;
                continue;
            }

            // old name 事件仅用于辅助（我们只用 new name 做实际更新）
            if (reason & USN_REASON_RENAME_OLD_NAME) != 0 && (reason & USN_REASON_RENAME_NEW_NAME) == 0
            {
                offset += record_len;
                continue;
            }

            let frn = unsafe { (*record_ptr).file_reference_number };
            if frn == state.root_frn {
                offset += record_len;
                continue;
            }
            let parent_frn = unsafe { (*record_ptr).parent_file_reference_number };
            let attrs = unsafe { (*record_ptr).file_attributes };
            let name_len_bytes = unsafe { (*record_ptr).file_name_length as usize };
            let name_off = unsafe { (*record_ptr).file_name_offset as usize };
            if name_len_bytes == 0 || name_off + name_len_bytes > record_len {
                offset += record_len;
                continue;
            }

            let name_ptr = unsafe { (record_ptr as *const u8).add(name_off) as *const u16 };
            let name_len_u16 = name_len_bytes / 2;
            let name_slice = unsafe { std::slice::from_raw_parts(name_ptr, name_len_u16) };
            let name = String::from_utf16_lossy(name_slice);
            if name.is_empty() {
                offset += record_len;
                continue;
            }

            events.push(UsnEvent {
                frn,
                parent_frn,
                attrs,
                reason,
                name,
            });

            if events.len() % 10_000 == 0 {
                progress.store(events.len(), Ordering::SeqCst);
            }
            if events.len() > 500_000 {
                unsafe {
                    CloseHandle(volume_handle);
                }
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "USN 增量变更过多，需要全量重建",
                ));
            }

            offset += record_len;
        }
    }

    unsafe {
        CloseHandle(volume_handle);
    }

    Ok(events)
}

fn apply_events_for_drive(entries: &mut Vec<FileEntry>, state: &UsnDriveState, events: Vec<UsnEvent>) {
    if events.is_empty() {
        return;
    }

    let drive = state.drive;
    let root_frn = state.root_frn;

    let mut frn_to_idx: HashMap<u64, usize> = HashMap::new();
    for (idx, entry) in entries.iter().enumerate() {
        if entry.drive == drive && entry.frn != 0 {
            frn_to_idx.insert(entry.frn, idx);
        }
    }

    for ev in events {
        // delete
        if (ev.reason & USN_REASON_FILE_DELETE) != 0 {
            let Some(&idx) = frn_to_idx.get(&ev.frn) else {
                continue;
            };
            let is_dir = entries[idx].is_dir;
            let old_path = entries[idx].path.clone();
            remove_entry_by_idx(entries, idx, &mut frn_to_idx);
            if is_dir && !old_path.is_empty() {
                let prefix = if old_path.ends_with('/') {
                    old_path
                } else {
                    format!("{old_path}/")
                };
                remove_entries_by_prefix(entries, drive, &prefix, &mut frn_to_idx);
            }
            continue;
        }

        // rename/move (new name)
        if (ev.reason & USN_REASON_RENAME_NEW_NAME) != 0 {
            if let Some(&idx) = frn_to_idx.get(&ev.frn) {
                let old_path = entries[idx].path.clone();
                let old_is_dir = entries[idx].is_dir;
                if let Some(new_path) = compose_path(entries, &frn_to_idx, drive, root_frn, ev.parent_frn, &ev.name)
                {
                    let entry = &mut entries[idx];
                    entry.name = ev.name;
                    entry.name_lower = lowercase_for_search(&entry.name);
                    entry.path = new_path.clone();
                    entry.path_lower = lowercase_for_search(&new_path);
                    entry.parent_frn = ev.parent_frn;
                    entry.is_dir = (ev.attrs & FILE_ATTRIBUTE_DIRECTORY) != 0;
                    entry.is_hidden =
                        (ev.attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM)) != 0;

                    if old_is_dir && !old_path.is_empty() && old_path != new_path {
                        let old_prefix = if old_path.ends_with('/') {
                            old_path
                        } else {
                            format!("{old_path}/")
                        };
                        let new_prefix = if new_path.ends_with('/') {
                            new_path
                        } else {
                            format!("{new_path}/")
                        };
                        update_entries_prefix(entries, drive, &old_prefix, &new_prefix);
                    }
                }
            } else if let Some(new_path) =
                compose_path(entries, &frn_to_idx, drive, root_frn, ev.parent_frn, &ev.name)
            {
                let is_dir = (ev.attrs & FILE_ATTRIBUTE_DIRECTORY) != 0;
                let is_hidden = (ev.attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM)) != 0;
                let name_lower = lowercase_for_search(&ev.name);
                let path_lower = lowercase_for_search(&new_path);
                let new_entry = FileEntry {
                    name: ev.name,
                    name_lower,
                    path: new_path,
                    path_lower,
                    drive,
                    frn: ev.frn,
                    parent_frn: ev.parent_frn,
                    size: u64::MAX,
                    modified_ms: 0,
                    is_dir,
                    is_hidden,
                };
                entries.push(new_entry);
                frn_to_idx.insert(ev.frn, entries.len() - 1);
            }
            continue;
        }

        // create
        if (ev.reason & USN_REASON_FILE_CREATE) != 0 {
            if frn_to_idx.contains_key(&ev.frn) {
                continue;
            }
            let Some(new_path) =
                compose_path(entries, &frn_to_idx, drive, root_frn, ev.parent_frn, &ev.name)
            else {
                continue;
            };

            let is_dir = (ev.attrs & FILE_ATTRIBUTE_DIRECTORY) != 0;
            let is_hidden = (ev.attrs & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM)) != 0;
            let name_lower = lowercase_for_search(&ev.name);
            let path_lower = lowercase_for_search(&new_path);
            let new_entry = FileEntry {
                name: ev.name,
                name_lower,
                path: new_path,
                path_lower,
                drive,
                frn: ev.frn,
                parent_frn: ev.parent_frn,
                size: u64::MAX,
                modified_ms: 0,
                is_dir,
                is_hidden,
            };
            entries.push(new_entry);
            frn_to_idx.insert(ev.frn, entries.len() - 1);
        }
    }
}

fn compose_path(
    entries: &[FileEntry],
    frn_to_idx: &HashMap<u64, usize>,
    drive: u8,
    root_frn: u64,
    parent_frn: u64,
    name: &str,
) -> Option<String> {
    let mut base = if parent_frn == root_frn {
        format!("{}:/", drive as char)
    } else {
        let idx = *frn_to_idx.get(&parent_frn)?;
        entries.get(idx)?.path.clone()
    };

    if !base.ends_with('/') {
        base.push('/');
    }
    base.push_str(name);
    Some(base)
}

fn remove_entry_by_idx(
    entries: &mut Vec<FileEntry>,
    idx: usize,
    frn_to_idx: &mut HashMap<u64, usize>,
) {
    let removed = entries.swap_remove(idx);
    if removed.frn != 0 {
        frn_to_idx.remove(&removed.frn);
    }
    if idx < entries.len() {
        let swapped = &entries[idx];
        if swapped.frn != 0 {
            frn_to_idx.insert(swapped.frn, idx);
        }
    }
}

fn remove_entries_by_prefix(
    entries: &mut Vec<FileEntry>,
    drive: u8,
    prefix: &str,
    frn_to_idx: &mut HashMap<u64, usize>,
) {
    let mut i = 0usize;
    while i < entries.len() {
        if entries[i].drive == drive && entries[i].path.starts_with(prefix) {
            remove_entry_by_idx(entries, i, frn_to_idx);
            continue;
        }
        i += 1;
    }
}

fn update_entries_prefix(entries: &mut [FileEntry], drive: u8, from_prefix: &str, to_prefix: &str) {
    for entry in entries.iter_mut() {
        if entry.drive != drive {
            continue;
        }
        if !entry.path.starts_with(from_prefix) {
            continue;
        }
        let rest = entry.path[from_prefix.len()..].to_string();
        entry.path = format!("{to_prefix}{rest}");
        entry.path_lower = lowercase_for_search(&entry.path);
    }
}

fn open_volume_handle(drive: char) -> io::Result<HANDLE> {
    let path = format!(r"\\.\{drive}:");
    let wide = to_wide_null(&path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null_mut(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

fn query_usn_journal(volume: HANDLE) -> io::Result<USN_JOURNAL_DATA_V0> {
    let mut data: USN_JOURNAL_DATA_V0 = unsafe { std::mem::zeroed() };
    let mut bytes: DWORD = 0;
    let ok: BOOL = unsafe {
        DeviceIoControl(
            volume,
            FSCTL_QUERY_USN_JOURNAL,
            ptr::null_mut(),
            0,
            &mut data as *mut _ as LPVOID,
            std::mem::size_of::<USN_JOURNAL_DATA_V0>() as DWORD,
            &mut bytes as *mut DWORD,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(data)
}

fn query_root_frn(drive: char) -> io::Result<u64> {
    let path = format!("{drive}:\\");
    let wide = to_wide_null(&path);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok: BOOL = unsafe { GetFileInformationByHandle(handle, &mut info as *mut _) };
    unsafe {
        CloseHandle(handle);
    }
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(((info.nFileIndexHigh as u64) << 32) | (info.nFileIndexLow as u64))
}

fn build_full_path(
    frn: u64,
    root_frn: u64,
    drive: char,
    nodes: &HashMap<u64, Node>,
    cache: &mut HashMap<u64, String>,
) -> Option<String> {
    if let Some(path) = cache.get(&frn) {
        return Some(path.clone());
    }

    let mut chain: Vec<u64> = Vec::new();
    let mut cur = frn;
    let mut depth = 0usize;

    let base = loop {
        if let Some(p) = cache.get(&cur) {
            break p.clone();
        }
        if cur == root_frn {
            break format!("{drive}:/");
        }

        let node = nodes.get(&cur)?;
        chain.push(cur);

        if node.parent == 0 || node.parent == cur {
            break format!("{drive}:/");
        }

        cur = node.parent;
        depth += 1;
        if depth > 4096 {
            return None;
        }
    };

    let mut path = base;
    for id in chain.iter().rev() {
        if *id == root_frn {
            cache.insert(*id, format!("{drive}:/"));
            continue;
        }
        let node = nodes.get(id)?;
        if !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(&node.name);
        cache.insert(*id, path.clone());
    }

    cache.get(&frn).cloned()
}

fn to_wide_null(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn lowercase_for_search(s: &str) -> String {
    if s.is_ascii() {
        s.to_ascii_lowercase()
    } else {
        s.to_lowercase()
    }
}
