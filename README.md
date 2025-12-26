基于 Rust 开发的 Windows 文件快速搜索器

作者：Serral

## v0.1.1

- 优化索引缓存机制（缓存更小，旧缓存自动升级）
- Windows: 使用 USN Journal 加速全盘枚举（NTFS）
- Windows: 使用 USN Journal 增量更新（避免频繁全盘扫描）
