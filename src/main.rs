mod api;
mod audio;
mod srt;
mod task;
mod web;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use task::{TaskExecutor, TaskManager};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(name = "ttsvii")]
#[command(about = "语音克隆批量处理工具", long_about = None)]
struct Args {
    /// 启动 Web UI 模式
    #[arg(long)]
    webui: bool,

    /// API 基础 URL
    #[arg(long, default_value = "http://183.147.142.111:63364")]
    api_url: String,

    /// 音频文件路径
    #[arg(short, long)]
    audio: Option<PathBuf>,

    /// SRT 字幕文件路径
    #[arg(short, long)]
    srt: Option<PathBuf>,

    /// 输出目录
    #[arg(short, long, default_value = "./output")]
    output: PathBuf,

    /// 最大并发数（API 请求）
    #[arg(long, default_value_t = 10)]
    max_concurrent: usize,

    /// 音频切分并发数（建议不超过 CPU 核心数）
    #[arg(long, default_value_t = 4)]
    split_concurrent: usize,

    /// 批次大小（每处理多少轮并发后休息）
    #[arg(long, default_value_t = 5)]
    batch_size: usize,

    /// 批次间休息时间（秒）
    #[arg(long, default_value_t = 1)]
    rest_duration: u64,

    /// 失败重试次数
    #[arg(long, default_value_t = 3)]
    retry_count: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 如果没有提供 audio 和 srt 参数，或者明确指定了 --webui，则启动 Web UI
    let should_start_webui = args.webui || (args.audio.is_none() && args.srt.is_none());

    if should_start_webui {
        return start_webui().await;
    }

    // 命令行模式
    let audio = args.audio.context("必须提供音频文件路径 (-a/--audio)")?;
    let srt = args.srt.context("必须提供 SRT 文件路径 (-s/--srt)")?;

    run_cli_mode(
        args.api_url,
        args.output,
        args.max_concurrent,
        args.split_concurrent,
        args.batch_size,
        args.rest_duration,
        args.retry_count,
        audio,
        srt,
    )
    .await
}

async fn start_webui() -> Result<()> {
    use axum::serve;
    use std::net::SocketAddr;
    use tokio::net::TcpListener;
    use tokio::time::{sleep, Duration};

    let app = web::create_router().await;
    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    let url = format!("http://{}", addr);

    println!("正在启动 Web UI 服务器...");
    println!("Web UI 地址: {}", url);

    let listener = TcpListener::bind(&addr).await?;

    // 在后台启动服务器
    let server_handle = tokio::spawn(async move {
        if let Err(e) = serve(listener, app.into_make_service()).await {
            eprintln!("Web 服务器错误: {}", e);
        }
    });

    // 等待服务器启动
    sleep(Duration::from_millis(500)).await;

    // 自动打开浏览器
    println!("正在打开浏览器...");
    if let Err(e) = open::that(&url) {
        eprintln!("无法自动打开浏览器: {}. 请手动访问: {}", e, url);
    }

    println!("按 Ctrl+C 停止服务器");

    // 等待服务器运行
    server_handle.await?;

    Ok(())
}

async fn run_cli_mode(
    api_url: String,
    output: PathBuf,
    max_concurrent: usize,
    split_concurrent: usize,
    batch_size: usize,
    rest_duration: u64,
    retry_count: u32,
    audio: PathBuf,
    srt: PathBuf,
) -> Result<()> {
    // 检查 ffmpeg
    audio::AudioSplitter::check_ffmpeg()
        .await
        .context("请确保已安装 ffmpeg 并在 PATH 中")?;

    // 生成本次运行的 UUID
    let task_uuid = Uuid::new_v4();
    let uuid_str = task_uuid.to_string();
    println!("任务 ID: {}", uuid_str);

    // 创建输出目录
    tokio::fs::create_dir_all(&output).await?;

    // 创建临时目录（在当前工作目录的 tmp/{uuid} 目录）
    let tmp_dir = std::env::current_dir()?.join("tmp").join(&uuid_str);
    tokio::fs::create_dir_all(&tmp_dir).await?;

    // 创建输出目录（output/{uuid} 目录）
    let output_task_dir = output.join(&uuid_str);
    tokio::fs::create_dir_all(&output_task_dir).await?;

    // 使用 defer 模式确保临时目录被清理
    let result = async {
        println!("正在解析 SRT 文件...");
        let srt_entries = srt::SrtParser::parse_file(&srt).context("解析 SRT 文件失败")?;
        println!("找到 {} 个字幕条目", srt_entries.len());

        if !srt_entries.is_empty() {
            println!("\n前 3 个字幕条目的时间信息:");
            for entry in srt_entries.iter().take(3) {
                let start_ms = entry.start_time.num_milliseconds();
                let end_ms = entry.end_time.num_milliseconds();
                println!(
                    "  条目 {}: 开始={}ms ({:.3}s), 结束={}ms ({:.3}s), 时长={}ms ({:.3}s), 文本=\"{}\"",
                    entry.index,
                    start_ms,
                    start_ms as f64 / 1000.0,
                    end_ms,
                    end_ms as f64 / 1000.0,
                    end_ms - start_ms,
                    (end_ms - start_ms) as f64 / 1000.0,
                    entry.text.trim().chars().take(30).collect::<String>()
                );
            }
        }

        // 创建任务管理器
        let task_manager = TaskManager::new(srt_entries, &audio, &tmp_dir, &output_task_dir)?;

        println!("正在切分音频文件（并发数: {}）...", split_concurrent);
        task_manager
            .prepare_audio_segments(&audio, split_concurrent)
            .await?;

        // 创建 API 客户端
        let api_client = std::sync::Arc::new(api::ApiClient::new(api_url.clone()));

        // 创建任务执行器
        let executor = std::sync::Arc::new(TaskExecutor::new(api_client, max_concurrent, retry_count));
        executor.set_total(task_manager.len() as u64);
        if retry_count > 0 {
            println!("失败重试次数: {}", retry_count);
        }

        println!(
            "开始处理 {} 个任务（并发数: {}, 批次大小: {}, 休息时间: {}s）...",
            task_manager.len(),
            max_concurrent,
            batch_size,
            rest_duration
        );

        // 批次处理任务
        let tasks = task_manager.get_tasks();
        let tasks_len = tasks.len();
        let mut all_results = Vec::new();

        // 按批次处理（每批次 = batch_size 轮并发）
        let batch_task_count = batch_size * max_concurrent;
        let mut batch_num = 0;

        for batch_start in (0..tasks_len).step_by(batch_task_count) {
            batch_num += 1;
            let batch_end = (batch_start + batch_task_count).min(tasks_len);
            let batch_tasks = &tasks[batch_start..batch_end];

            println!(
                "\n批次 {}/{}: 处理任务 {}-{}",
                batch_num,
                (tasks_len + batch_task_count - 1) / batch_task_count,
                batch_start + 1,
                batch_end
            );

            // 执行当前批次（给每个任务添加总体超时保护，防止卡住）
            let futures: Vec<_> = batch_tasks
                .iter()
                .map(|task| {
                    let task = task.clone();
                    let executor = executor.clone();
                    async move {
                        // 给整个任务添加 15 分钟超时（比内部 API 调用的 10 分钟超时稍长）
                        tokio::time::timeout(
                            std::time::Duration::from_secs(900), // 15 分钟
                            executor.execute_task(task),
                        )
                        .await
                        .unwrap_or_else(|_| {
                            Err(anyhow::anyhow!("任务执行超时（超过 15 分钟）"))
                        })
                    }
                })
                .collect();

            println!("开始执行批次任务...");
            let batch_results = futures::future::join_all(futures).await;
            println!("批次任务执行完成，开始收集结果...");
            all_results.extend(batch_results);

            // 显示实时统计
            let (success, failure) = executor.get_stats();
            let completed = success + failure;
            println!(
                "实时统计: 进度 {}/{} | 成功: {} | 失败: {}",
                completed, tasks_len, success, failure
            );

            // 如果不是最后一批，休息一下
            if batch_end < tasks_len {
                println!("休息 {} 秒...", rest_duration);
                tokio::time::sleep(std::time::Duration::from_secs(rest_duration)).await;
            }
        }

        let results = all_results;

        println!("\n所有批次任务已完成，开始检查结果...");
        // 检查结果
        let mut errors = Vec::new();
        let mut skipped = Vec::new();
        for (i, result) in results.into_iter().enumerate() {
            if let Err(e) = result {
                let err_msg = e.to_string();
                // 区分可跳过的错误（空文本、空音频）和真正的 API 错误
                if err_msg.contains("文本内容为空")
                    || err_msg.contains("音频文件为空")
                    || err_msg.contains("音频文件不存在")
                    || err_msg.contains("音频文件无效")
                    || err_msg.contains("切分的音频文件无效")
                {
                    skipped.push((i, err_msg));
                } else {
                    errors.push((i, e));
                }
            }
        }

        executor.finish();

        if !skipped.is_empty() {
            println!("跳过 {} 个无效任务（空文本或空音频）", skipped.len());
        }

        if !errors.is_empty() {
            eprintln!("有 {} 个任务失败:", errors.len());
            // 只显示前 20 个错误，避免输出过多
            let display_count = errors.len().min(20);
            for (i, err) in errors.iter().take(display_count) {
                eprintln!("  任务 {}: {}", i, err);
            }
            if errors.len() > 20 {
                eprintln!("  ... 还有 {} 个错误未显示", errors.len() - 20);
            }

            // 显示统计信息
            let success_count = tasks.len() - errors.len() - skipped.len();
            println!(
                "\n统计: 成功: {} 个, 失败: {} 个, 跳过: {} 个",
                success_count,
                errors.len(),
                skipped.len()
            );

            anyhow::bail!("部分任务执行失败");
        }

        println!("\n所有任务执行完成，开始收集音频文件...");

        // 收集所有合成的音频文件及其对应的 SRT 条目（只包含实际存在的文件）
        let mut audio_files = Vec::new();
        let mut srt_entries_for_merge = Vec::new();
        // 按索引排序 tasks
        let mut sorted_tasks: Vec<_> = tasks.iter().collect();
        sorted_tasks.sort_by_key(|task| task.entry.index);
        for task in sorted_tasks {
            if task.output_path.exists() {
                let metadata = tokio::fs::metadata(&task.output_path).await?;
                if metadata.len() > 0 {
                    audio_files.push(task.output_path.clone());
                    srt_entries_for_merge.push(task.entry.clone());
                }
            }
        }

        if audio_files.is_empty() {
            anyhow::bail!("没有有效的合成音频文件可以合并");
        }

        println!("找到 {} 个有效的音频文件用于合并", audio_files.len());

        // 合并音频（保存到 output/{uuid}.wav）
        let final_output = output.join(format!("{}.wav", uuid_str));
        println!("开始合并 {} 个音频文件到: {}", audio_files.len(), final_output.display());
        println!("（这可能需要一些时间，请耐心等待...）");
        // 给合并操作添加超时保护（最多 30 分钟）
        tokio::time::timeout(
            std::time::Duration::from_secs(1800), // 30 分钟
            audio::AudioSplitter::merge_audio(&audio_files, &final_output),
        )
        .await
        .context("合并音频超时（超过 30 分钟）")??;

        println!("完成！原始合并音频已保存到: {}", final_output.display());

        // 生成根据 SRT 时间戳变速后的合并音频
        let timed_output = output.join(format!("{}_timed.wav", uuid_str));
        println!("\n开始生成根据 SRT 时间戳变速后的合并音频...");
        println!("目标文件: {}", timed_output.display());
        println!("（这可能需要一些时间，请耐心等待...）");
        tokio::time::timeout(
            std::time::Duration::from_secs(1800), // 30 分钟
            audio::AudioSplitter::merge_audio_with_timing(
                &audio_files,
                &srt_entries_for_merge,
                &timed_output,
            ),
        )
        .await
        .context("合并变速音频超时（超过 30 分钟）")??;

        println!("完成！变速合并音频已保存到: {}", timed_output.display());

        Ok::<(), anyhow::Error>(())
    }
    .await;
    result
}
