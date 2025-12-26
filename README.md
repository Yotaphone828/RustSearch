<p align="center">
  <img src="https://github.com/user-attachments/assets/e7b0d75d-6cb0-4368-8a24-031282087c67" alt="Logo" width="120" height="120">
</p>

<h3 align="center">RustSearch</h3>

<p align="center">
  基于Rust的windows文件搜索器，快速索引（3-8s），极速搜索
  <br>
  <a href="https://your-doc-link.com">查看文档</a> ·
  <a href="https://github.com/your-username/your-repo/issues">报告 Bug</a>
</p>

## 索引原理（Windows）

- 默认不写入本地索引缓存文件
- NTFS 下通过 USN Journal 的 `FSCTL_ENUM_USN_DATA` 枚举（等价于从 MFT 视角获取全盘文件记录），构建内存索引
- 为加速启动，默认只保留文件名/FRN/父FRN 等必要字段；完整路径在展示/打开少量结果时按需拼接
- 若卷不是 NTFS、USN 不可用或权限不足，会自动回退到常规目录遍历（`walkdir`）
- 若统计里出现 `USN 枚举失败: 拒绝访问 (code=5)`，通常需要以管理员身份运行（卷管理权限）

## 管理员权限提示

当检测到 `USN 枚举失败 (code=5)` 时，程序会弹出提示窗口，并提供“以管理员身份重启”按钮（会触发 Windows 的 UAC 授权弹窗）。

默认行为：程序启动时会尝试触发 UAC 以管理员身份重启（可通过环境变量 `RUSTSEARCH_SKIP_ELEVATE=1` 跳过）。
