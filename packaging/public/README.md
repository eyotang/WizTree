# WizTree

原生磁盘空间分析工具，采用 Rust 与 GPUI Kit 开发。提供目录树、文件列表、类型统计和矩形空间图，支持 Windows、macOS 和 Linux 的独立扫描后端。

## 界面预览

![WizTree 界面预览](docs/media/wiztree-preview.png)

*截图使用固定的虚构演示数据；磁盘、容量和文件名均为示例，不包含本机目录或个人信息。*

## 安装

- **Windows**：解压对应 ZIP，双击 `WizTree.exe`。
- **macOS**：打开对应芯片版本的 DMG，将 `WizTree.app` 拖入“应用程序”，然后从启动台（Launchpad）打开。支持 macOS 14 及以上。
- **Linux**：解压对应包，运行 `./install.sh` 安装到当前用户的应用目录，随后可从应用启动器打开。

安装包按平台构建。请以实际附带的文件和发布说明为准；源码与工作流存在不表示所有平台已完成实机验收。

## 使用

选择磁盘或文件夹开始扫描；开始新扫描会清空旧结果，完成后显示本轮数据。树表单击选择、双击目录展开或折叠；Enter 进入所选目录。空间图单击查看详情，双击进入目录。前进/后退保留浏览位置，快照内导航不重新扫描。

按 Space 标记文件或文件夹，在“复核”侧栏检查后，可明确确认移到系统回收站。搜索支持通配符、排除规则、最小大小与文件类型筛选；同名同大小仅为重复候选。

| 操作 | 快捷键 |
| --- | --- |
| 选择文件夹 | Ctrl/Cmd+O |
| 搜索 / 编辑路径 | Ctrl/Cmd+F / Ctrl/Cmd+L |
| 重新扫描 | Ctrl/Cmd+R |
| 后退 / 前进 / 上级 | Alt+Left / Alt+Right / Alt+Up |
| 标记 / 清理前复核 | Space / Ctrl/Cmd+Shift+M |
| 停止扫描 / 退出编辑 | Esc |

## 平台后端

Windows 使用批量目录元数据读取，NTFS MFT 为实验选项；macOS 使用批量属性读取，不支持的文件系统自动兼容；Linux 使用缓冲目录枚举与目录句柄相对查询。后台扫描支持取消，程序不依赖扫描数据库。

分析读取元数据，不上传扫描结果。最近路径、主题和本地诊断信息保存在当前用户的应用设置目录；导出和回收站操作仅在用户触发时进行。

## 开发与验证

需要 Rust stable、对应平台的原生开发工具及可用的图形驱动。GPUI Kit 固定为 0.7.3；依赖由 Cargo.lock 锁定。

```sh
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
python3 tools/build_release.py
```

Windows 使用 Visual Studio C++ 构建工具和 Windows SDK；macOS 使用完整 Xcode 及 Metal 工具链；Linux 开发依赖见构建工作流。

## 当前边界

当前为 0.1.0 预览版。首次扫描性能仍有待优化，不能承诺与其他工具在同一时间完成扫描。MFT、超大规模目录和复杂文件系统口径仍需进一步验证。占用合计不保证等于删除后实际释放空间。

Windows 工作区测试和 Linux WSL 核心测试已通过；macOS 核心的 Intel/Apple Silicon 编译检查通过，macOS 实机扫描以及 macOS/Linux 桌面实机尚待验收。正式 macOS 分发还需要合适的 Developer ID 签名与公证。

详见 [平台说明](docs/platforms.md)、[发布与安装](docs/releasing.md) 和 [验证范围](docs/validation.md)。

本项目代码采用 MIT 许可证。第三方组件的许可证单独保留。交互设计参考开源 [DiskTree](https://github.com/huacnlee/gpui-shell-benchs/tree/c0cb9ed9a154cee4ae958a1030630c1855f70f8b/apps/disktree)，组件来自 [GPUI Kit](https://github.com/longbridge/gpui-kit)。本项目为独立实现，未使用原版程序的代码、二进制或图像资源。
