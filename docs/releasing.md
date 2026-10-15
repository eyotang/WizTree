# 发布与安装

## 发布目录

打包脚本只从白名单构建公开源码树，不打包本机设置、Git 历史、测试输出、逆向资料或开发诊断。源码包同时携带构建工具和依赖锁文件。原工作目录中的其他资料不会因此删除。

```sh
python3 tools/build_release.py
python3 tools/package_release.py --source --platform windows --binary-dir target/distributable/x86_64-pc-windows-msvc/release
```

在 macOS/Linux 上使用各自原生构建机。macOS 构建脚本会输出中性的临时构建目录，用于避免 Metal 编译器另行记录开发路径。将输出目录传给打包命令：

```sh
python3 tools/package_release.py --platform macos --arch arm64 --binary-dir <build-output>
python3 tools/package_release.py --platform linux --arch x86_64 --binary-dir <build-output>
```

macOS Intel 使用 `--arch x86_64`。工作流为 Windows、Linux、macOS Intel 和 Apple Silicon 分别构建，不将其他系统的二进制复制成目标平台安装包。

## macOS 安装体验

DMG 内包含原生 `WizTree.app` 与指向“应用程序”的入口。打开 DMG，将应用拖入“应用程序”，再从启动台（Launchpad）或 Finder 打开。无需终端启动。

应用包含自身 Info.plist、图标、说明和必要的第三方许可证。构建机负责生成 ICNS、验证应用签名及磁盘映像；macOS 安装包不能仅在 Windows 上验收。

## macOS 正式分发

默认打包使用 ad-hoc 签名，仅用于构建验证。正式分发需在 Mac 上配置适合本项目的个人 Developer ID，使用 `MACOS_SIGNING_IDENTITY` 指定身份；不要默认使用含工作单位信息的签名证书。配置好系统钥匙串的公证凭据后，设置 `MACOS_NOTARY_PROFILE`，脚本将提交公证并附加、验证票据。不要把证书、口令或账号写入源码。

公证需要网络与开发者账号；未执行时不得把安装包标记为已公证，也不得声称已通过 Gatekeeper 验收。依据：[Apple 分发打包](https://developer.apple.com/documentation/xcode/packaging-mac-software-for-distribution)、[Apple 公证流程](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)。

## 隐私与许可证检查

编译阶段重映射源码路径，分发时不附带调试符号。压缩包移除构建主机的所有者信息和原文件时间；工具检查开发路径、当前开发用户、内部地址、密钥标记及私有目录，失败时停止打包。不要直接压缩整个工作目录。

第三方组件清单与许可证保留在程序包内。必要的上游版权方名称属于开源许可声明，不代表项目与其存在工作单位关系。没有单独许可证文件的组件以及 MPL 组件附带原始源码归档及获取地址。

`SHA256SUMS.txt` 用于检查传输完整性，不代表代码签名。GitHub 的“Build distribution packages”工作流只构建和上传 CI 产物，不自动创建公开 Release；确认各端实测结果后再正式发布。

## 安装包验收

打包后运行 `tools/verify_package.py`，参数与打包命令的 `--platform`、`--arch`、`--binary-dir` 及 `--out` 一致。Windows/Linux 从实际压缩包解包后运行扫描器，核对跨批次目录、中文名称、CSV、不同线程数及缺失根目录。Linux 还执行隔离用户安装；macOS 挂载只读 DMG，检查应用结构、架构、签名及 Applications 入口，并验证同次原生构建的扫描器。报告明确区分安装包检查、桌面交互验收和首次扫描性能验收；前者通过不代替后两项。
