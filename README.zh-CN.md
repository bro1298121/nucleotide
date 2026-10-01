# Nucleotide

**Helix 的原生图形界面**

Nucleotide 是 [Helix](https://helix-editor.com/) 模态编辑器的高性能图形界面，将基于终端的模态编辑能力带入现代原生 GUI。

## 站在巨人的肩膀上

没有以下这些出色的项目，就不会有 Nucleotide：

- **[Helix](https://helix-editor.com/)** —— 驱动我们编辑引擎的强大模态编辑器
- **[GPUI](https://github.com/zed-industries/zed)** —— Zed 那快如闪电的 GPU 加速 UI 框架
- **[helix-gpui](https://github.com/polachok/helix-gpui)** —— 我们 fork 的原始项目，由 @polachok 创建

我们深深感谢这些项目及其维护者，是它们让 Nucleotide 成为可能。

## 功能特性

目前，Nucleotide 为 Helix 提供了一个原生 GUI 封装，具备以下能力：
- 原生支持 macOS/Linux/Windows
- 通过 GPUI 实现 GPU 加速渲染
- 文件树侧边栏
- 集成终端（规划中）
- 完整支持 Helix 键位绑定

## 安装

### 从源码构建

首先安装 Rust stable 和 Zig 0.15.2。构建基于 Ghostty 的集成终端需要 Zig。

```bash
cargo build --release
./target/release/nucl
```

仓库本地的 Cargo 配置在构建期间禁用了 Helix 的自动语法获取。当需要更新打包的运行时语法时，请使用 `nucl --grammar fetch` / `nucl --grammar build` 或打包脚本。

### macOS 打包

```bash
./scripts/bundle-mac.sh
open Nucleotide.app
```

### Velopack 包

Nucleotide 为 macOS 发布 Velopack 安装程序和更新源。在 Windows 上，唯一的安装产物是便携式 `*-Portable.zip` 压缩包；将整个压缩包解压到可写目录后启动 `Nucleotide.exe`。发布内容还包含应用内更新器使用的源和包。

在本地构建 Windows 便携包之前，请先安装 Rust stable、Zig 0.15.2、.NET 8 SDK 和 Velopack CLI。

```powershell
cargo build --release -p nucleotide --bins
git clone --depth 1 --branch 25.07.1 https://github.com/helix-editor/helix.git helix-temp
try {
  .\scripts\setup-windows-runtime.cmd -RuntimeSource helix-temp\runtime -NuclExe target\release\nucl-grammar.exe
} finally {
  Remove-Item -LiteralPath helix-temp -Recurse -Force
}
dotnet tool update -g vpk --version 1.2.0
.\scripts\package-velopack.ps1 -RequireRemoteHelpers
```

See `docs/windows_install.md` for local Windows Velopack build and install
notes. See `docs/application_updates.md` for the in-app update flow and release
pipeline.

有关本地 Windows Velopack 构建和安装说明，请参阅 `docs/windows_install.md`。有关应用内更新流程和发布流水线，请参阅 `docs/application_updates.md`。

## 开发环境搭建

### 安装 Git 钩子

为确保代码格式一致，请安装 pre-commit 钩子：

```bash
./scripts/install-hooks.sh
```

这将在每次提交前自动执行 `cargo fmt` 检查。

## 配置

Nucleotide 使用 Helix 的平台配置目录。它会在此目录中查找 `nucleotide.toml`，并回退到 Helix 的 `config.toml` 作为编辑器设置。

- Linux/macOS：通常为 `~/.config/helix/nucleotide.toml`
- Windows：`%APPDATA%\helix\nucleotide.toml`

示例 GUI 配置请参阅 `docs/examples/nucleotide.example.toml`。

## 许可证

MPL-2.0（与 Helix 相同）