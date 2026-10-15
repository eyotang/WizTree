# 平台实现与支持状态

| 平台 | 目录扫描 | 当前验证 |
| --- | --- | --- |
| Windows x64 | 原生批量目录属性读取；实验性 NTFS MFT | 工作区测试和桌面运行通过 |
| macOS Apple Silicon / Intel | getattrlistbulk，按返回属性解码；不支持时回退 POSIX | 两种架构原生测试与 DMG 检查通过；Apple Silicon 原生交互检查通过 |
| Ubuntu 24.04+ x64（DEB） | 缓冲 readdir 与相对目录句柄的 fstatat | 原生测试、Xvfb 桌面交互、安装包运行及 WSLg 启动通过 |

树结构、查询、进度和 UI 共用；系统元数据读取分平台实现。MFT 菜单仅在 Windows 展示。普通符号链接、Windows 重解析点及特殊设备跳过，读取错误计数并保留有限样本。

Linux/POSIX 后备用分配块数计算占用。macOS 的逻辑大小对应数据叉，分配量包含文件叉。硬链接、APFS 克隆与快照、Btrfs reflink 等共享空间不能简单累加为实际可回收量。当前不提供一致性快照或整棵目录树的无竞争保证。

macOS 读取受系统文件访问权限约束；Linux、Windows 读取也遵守当前用户权限。应用不依赖系统索引数据库，不读取普通文件内容进行磁盘占用分析。
