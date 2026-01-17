# TTSVII - 语音克隆批量处理工具

一个高性能、跨平台的 Rust 程序，用于批量处理语音克隆任务。根据 SRT 字幕文件的时间片段切分音频，并发调用 API 进行语音合成，最后合并为完整音频。

## 功能特性

- ✅ **跨平台支持** - 支持 Windows、Linux、macOS
- ✅ **高并发处理** - 异步多线程，可自定义并发数
- ✅ **任务制架构** - 每个字幕片段作为独立任务处理
- ✅ **自动音频切分** - 根据 SRT 时间戳自动切分音频
- ✅ **自动合并** - 合成后自动合并为完整音频
- ✅ **进度显示** - 实时显示处理进度
- ✅ **自定义配置** - 支持自定义 API URL 和输出目录

## 系统要求

- Rust 1.70+ 
- ffmpeg（必须安装在系统 PATH 中）

### 安装 ffmpeg

**Linux (Arch):**
```bash
sudo pacman -S ffmpeg
```

**Linux (Ubuntu/Debian):**
```bash
sudo apt-get install ffmpeg
```

**macOS:**
```bash
brew install ffmpeg
```

**Windows:**
下载并安装 [ffmpeg](https://ffmpeg.org/download.html)，确保添加到 PATH。

## 安装

```bash
# 克隆仓库
git clone <repository-url>
cd ttsvii

# 编译
cargo build --release

# 可执行文件位于 target/release/ttsvii
```

## 使用方法

```bash
ttsvii --audio <音频文件> --srt <SRT文件> [选项]
```

### 基本示例

```bash
ttsvii \
  --audio input.wav \
  --srt subtitles.srt \
  --output ./output \
  --api-url http://183.147.142.111:63364 \
  --max-concurrent 10
```

### 命令行参数

- `--audio, -a`: 输入的音频文件路径（必需）
- `--srt, -s`: SRT 字幕文件路径（必需）
- `--output, -o`: 输出目录（默认: `./output`）
- `--api-url`: API 基础 URL（默认: `http://183.147.142.111:63364`）
- `--max-concurrent`: 最大并发任务数（默认: 10）

## 工作流程

1. **解析 SRT 文件** - 提取所有字幕条目和时间戳**
2. **切分音频** - 根据每个字幕条目的时间范围切分原始音频
3. **并发处理** - 为每个字幕片段创建任务，并发调用 API
   - 使用切分的音频作为 `speaker_audio`（克隆音色参考）
   - 使用切分的音频作为 `emotion_audio`（情绪参考）
   - 使用字幕文本作为 `text`（要合成的文本）
4. **下载合成音频** - 保存每个任务合成的音频到临时目录
5. **合并音频** - 按顺序合并所有合成的音频片段
6. **输出结果** - 最终音频保存为 `final_output.wav`

## 输出文件结构

```
output/
├── speaker_1.wav          # 第1个片段的参考音频
├── emotion_1.wav          # 第1个片段的情绪参考音频
├── synthesized_1.wav      # 第1个片段合成的音频
├── speaker_2.wav
├── emotion_2.wav
├── synthesized_2.wav
├── ...
└── final_output.wav       # 最终合并的完整音频
```

## API 接口说明

程序调用 `/synthesize` 端点，发送以下参数：

- `text` (必需): 要合成的文本
- `speaker_audio` (可选): 克隆音色参考音频文件
- `emotion_audio` (可选): 情绪参考音频文件
- `prompt_audio` (可选): 音色 URL
- `duration_factor` (可选): 时长缩放因子（默认 1.72）
- `target_duration` (可选): 精确时长控制（秒）

## 性能优化

- 使用 `tokio` 异步运行时实现高并发
- 使用信号量（Semaphore）控制并发数，避免过载
- 并行切分音频片段，减少等待时间
- 使用 `rayon` 进行 CPU 密集型任务的并行处理

## 错误处理

- 所有任务失败会显示详细错误信息
- 部分任务失败不会中断整个流程
- 最终会报告失败任务的数量和详情

## 许可证

MIT License

## 贡献

欢迎提交 Issue 和 Pull Request！
