# GitHub Actions 工作流说明

本项目包含两个 GitHub Actions 工作流：

## 1. CI 工作流 (`ci.yml`)

完整的持续集成工作流，包括：

- **代码检查**：格式化检查、Clippy 检查、编译检查
- **测试**：在多个平台上运行测试（Linux、Windows、macOS）
- **构建**：为多个平台构建发布版本
- **发布**：当创建 GitHub Release 时，自动打包并上传构建产物

### 触发条件

- 推送到 `main`、`master` 或 `develop` 分支
- 针对这些分支的 Pull Request
- 创建 GitHub Release

### 支持的平台

- Linux: x86_64 (GNU, musl), aarch64
- Windows: x86_64 (MSVC, GNU)
- macOS: x86_64, aarch64 (Apple Silicon)

## 2. 构建工作流 (`build.yml`)

简化的构建工作流，专注于构建发布版本：

- **手动触发**：可以通过 GitHub Actions 界面手动触发
- **标签触发**：当推送以 `v` 开头的标签时自动触发（如 `v1.0.0`）

### 支持的平台

- Linux: x86_64, aarch64
- Windows: x86_64
- macOS: x86_64, aarch64

## 使用说明

### 手动触发构建

1. 前往 GitHub 仓库的 Actions 页面
2. 选择 "Build" 工作流
3. 点击 "Run workflow" 按钮

### 创建发布版本

1. 创建并推送一个标签：
   ```bash
   git tag v1.0.0
   git push origin v1.0.0
   ```

2. 或者在 GitHub 上创建 Release，CI 会自动构建并上传所有平台的二进制文件

### 下载构建产物

构建完成后，可以在 Actions 页面下载对应平台的构建产物：
- Linux: `.tar.gz` 文件
- Windows: `.zip` 文件
- macOS: `.tar.gz` 文件

每个文件都包含对应的 SHA256 校验和文件。

## 注意事项

- 交叉编译需要 `cross` 工具，CI 会自动安装
- 构建产物会保留 7-30 天（取决于工作流）
- 首次构建可能需要较长时间，后续构建会使用缓存加速
