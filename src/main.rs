mod api;
mod audio;
mod srt;
mod task;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
use task::{TaskExecutor, TaskManager};

#[derive(Parser, Debug)]
#[command(name = "ttsvii")]
#[command(about = "语音克隆批量处理工具", long_about = None)]
struct Args {
    /// API 基础 URL
    #[arg(long, default_value = "http://183.147.142.111:63364")]
    api_url: String,

    /// 音频文件路径
    #[arg(short, long)]
    audio: PathBuf,

    /// SRT 字幕文件路径
    #[arg(short, long)]
    srt: PathBuf,

    /// 输出目录
    #[arg(short, long, default_value = "./output")]
    output: PathBuf,

    /// 最大并发数
    #[arg(long, default_value_t = 10)]
    max_concurrent: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // 检查 ffmpeg
    audio::AudioSplitter::check_ffmpeg()
        .await
        .context("请确保已安装 ffmpeg 并在 PATH 中")?;

    // 创建输出目录
    tokio::fs::create_dir_all(&args.output).await?;

    println!("正在解析 SRT 文件...");
    let srt_entries = srt::SrtParser::parse_file(&args.srt).context("解析 SRT 文件失败")?;
    println!("找到 {} 个字幕条目", srt_entries.len());

    // 创建任务管理器
    let task_manager = TaskManager::new(srt_entries, &args.audio, &args.output)?;

    println!("正在切分音频文件...");
    task_manager.prepare_audio_segments(&args.audio).await?;
    println!("音频切分完成");

    // 创建 API 客户端
    let api_client = std::sync::Arc::new(api::ApiClient::new(args.api_url.clone()));

    // 创建任务执行器
    let executor = TaskExecutor::new(api_client, args.max_concurrent);
    executor.set_total(task_manager.len() as u64);

    println!(
        "开始处理 {} 个任务（并发数: {}）...",
        task_manager.len(),
        args.max_concurrent
    );

    // 执行所有任务
    let tasks = task_manager.get_tasks();
    let futures: Vec<_> = tasks
        .iter()
        .map(|task| executor.execute_task(task.clone()))
        .collect();

    let results = futures::future::join_all(futures).await;

    // 检查结果
    let mut errors = Vec::new();
    for (i, result) in results.into_iter().enumerate() {
        if let Err(e) = result {
            errors.push((i, e));
        }
    }

    executor.finish();

    if !errors.is_empty() {
        eprintln!("有 {} 个任务失败:", errors.len());
        for (i, err) in errors {
            eprintln!("  任务 {}: {}", i, err);
        }
        anyhow::bail!("部分任务执行失败");
    }

    println!("所有任务执行完成，正在合并音频...");

    // 收集所有合成的音频文件
    let mut audio_files: Vec<PathBuf> = tasks.iter().map(|task| task.output_path.clone()).collect();

    // 按索引排序
    audio_files.sort();

    // 合并音频
    let final_output = args.output.join("final_output.wav");
    audio::AudioSplitter::merge_audio(&audio_files, &final_output).await?;

    println!("完成！最终音频已保存到: {}", final_output.display());

    Ok(())
}
