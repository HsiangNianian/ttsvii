use crate::api::ApiClient;
use crate::audio::AudioSplitter;
use crate::srt::SrtEntry;
use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

struct RateLimiter {
    sem: Arc<Semaphore>,
    capacity: usize,
}

impl RateLimiter {
    fn new(rate_per_sec: usize) -> Self {
        // 令牌桶容量 = 每秒速率，避免过度囤积
        let cap = rate_per_sec.max(1);
        Self {
            sem: Arc::new(Semaphore::new(cap)),
            capacity: cap,
        }
    }

    fn start_refill(self: Arc<Self>, interval: Duration) {
        // 后台定时补充令牌，平滑突发
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                let available = self.sem.available_permits();
                if available < self.capacity {
                    self.sem.add_permits(1);
                }
            }
        });
    }

    async fn acquire(&self) {
        let _ = self.sem.acquire().await;
    }
}

#[derive(Default)]
struct Metrics {
    latencies_ms: Mutex<Vec<f64>>, // 最近窗口的请求时延
    success: AtomicU64,
    failure: AtomicU64,
}

impl Metrics {
    fn record_latency(&self, ms: f64) {
        let mut guard = self.latencies_ms.lock().unwrap();
        guard.push(ms);
        // 限制窗口大小，保留最新的 200 条
        if guard.len() > 200 {
            let drop = guard.len() - 200;
            guard.drain(0..drop);
        }
    }

    fn snapshot(&self) -> (f64, f64, u64, u64) {
        let guard = self.latencies_ms.lock().unwrap();
        let mut data = guard.clone();
        data.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let p = |q: f64| -> f64 {
            if data.is_empty() {
                return 0.0;
            }
            let idx = ((q * (data.len() as f64 - 1.0)).round() as usize).min(data.len() - 1);
            data[idx]
        };
        let p50 = p(0.5);
        let p95 = p(0.95);
        (
            p50,
            p95,
            self.success.load(Ordering::Relaxed),
            self.failure.load(Ordering::Relaxed),
        )
    }
}

#[derive(Clone)]
pub struct Task {
    pub entry: SrtEntry,
    pub tmp_audio: PathBuf,
    pub output_path: PathBuf,
}

pub struct TaskExecutor {
    api_client: Arc<ApiClient>,
    semaphore: Arc<Semaphore>,
    progress: Arc<ProgressBar>,
    success_count: Arc<std::sync::atomic::AtomicU64>,
    failure_count: Arc<std::sync::atomic::AtomicU64>,
    retry_count: u32,
    rate_limiter: Arc<RateLimiter>,
    metrics: Arc<Metrics>,
}

impl TaskExecutor {
    pub fn new(api_client: Arc<ApiClient>, max_concurrent: usize, retry_count: u32) -> Self {
        let progress = ProgressBar::new(0);
        progress.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} ({eta})")
                .unwrap()
                .progress_chars("#>-"),
        );

        // 令牌桶速率设为并发的 2 倍，避免突发过大
        let rate_limit = (max_concurrent * 2).max(1);
        let rate_limiter = Arc::new(RateLimiter::new(rate_limit));
        // 每 (1000 / rate_limit) ms 补充 1 个令牌
        let refill_interval = Duration::from_millis((1000 / rate_limit as u64).max(10));
        rate_limiter.clone().start_refill(refill_interval);

        Self {
            api_client,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            progress: Arc::new(progress),
            success_count: Arc::new(AtomicU64::new(0)),
            failure_count: Arc::new(AtomicU64::new(0)),
            retry_count,
            rate_limiter,
            metrics: Arc::new(Metrics::default()),
        }
    }

    pub async fn execute_task(&self, task: Task) -> Result<()> {
        let _permit = self.semaphore.acquire().await.unwrap();

        // 执行任务（带重试逻辑）
        let mut last_error = None;
        let max_attempts = self.retry_count + 1; // 初始尝试 + 重试次数

        for attempt in 1..=max_attempts {
            let result = self.execute_inner(&task).await;

            match result {
                Ok(_) => {
                    // 成功，更新统计并返回
                    self.success_count.fetch_add(1, Ordering::Relaxed);
                    self.metrics.success.fetch_add(1, Ordering::Relaxed);
                    self.progress.inc(1);
                    return Ok(());
                }
                Err(e) => {
                    last_error = Some(e);

                    // 如果不是最后一次尝试，等待后重试
                    if attempt < max_attempts {
                        let delay_ms =
                            classify_backoff_delay(&last_error.as_ref().unwrap(), attempt);
                        tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                    // 最后一次尝试也失败，跳出循环
                    break;
                }
            }
        }

        // 所有尝试都失败，标记为失败
        self.failure_count.fetch_add(1, Ordering::Relaxed);
        self.metrics.failure.fetch_add(1, Ordering::Relaxed);
        self.progress.inc(1);
        Err(last_error.expect("应该有错误信息"))
    }

    pub fn get_stats(&self) -> (u64, u64) {
        (
            self.success_count.load(Ordering::Relaxed),
            self.failure_count.load(Ordering::Relaxed),
        )
    }

    pub fn metrics_snapshot(&self) -> (f64, f64, u64, u64) {
        self.metrics.snapshot()
    }

    async fn execute_inner(&self, task: &Task) -> Result<()> {
        // 验证输入：跳过空文本
        let text = task.entry.text.trim();
        if text.is_empty() {
            anyhow::bail!("文本内容为空，跳过此任务");
        }

        // 验证音频文件是否存在且有效
        if !task.tmp_audio.exists() {
            anyhow::bail!("音频文件不存在: {:?}", task.tmp_audio);
        }

        let speaker_size = tokio::fs::metadata(&task.tmp_audio).await?.len();
        // WAV 文件头部至少 44 字节，如果文件太小说明没有实际音频数据
        if speaker_size == 0 {
            anyhow::bail!("音频文件为空: {:?}", task.tmp_audio);
        }
        if speaker_size <= 44 {
            anyhow::bail!(
                "音频文件无效（文件大小: {} 字节，可能只有文件头）: {:?}",
                speaker_size,
                task.tmp_audio
            );
        }

        // 令牌桶平滑发请求
        self.rate_limiter.acquire().await;

        // 调用 API 合成音频（移除超时限制，允许 API 长时间处理）
        let start = Instant::now();
        let audio_data = self
            .api_client
            .synthesize(
                text,
                Some(&task.tmp_audio),
                Some(&task.tmp_audio),
                None,
                None,
                None,
            )
            .await?;
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        self.metrics.record_latency(elapsed);

        // 验证返回的音频数据
        if audio_data.is_empty() {
            anyhow::bail!("API 返回的音频数据为空");
        }

        // 保存合成的音频
        tokio::fs::write(&task.output_path, audio_data).await?;

        Ok(())
    }

    pub fn set_total(&self, total: u64) {
        self.progress.set_length(total);
    }

    pub fn finish(&self) {
        self.progress.finish_with_message("完成");
    }
}

pub struct TaskManager {
    tasks: Vec<Task>,
}

impl TaskManager {
    pub fn new(
        srt_entries: Vec<SrtEntry>,
        _audio_path: &Path,
        tmp_dir: &Path,
        output_dir: &Path,
    ) -> Result<Self> {
        let mut tasks = Vec::new();
        let mut skipped = 0;

        for entry in srt_entries {
            // 跳过空文本的条目
            if entry.text.trim().is_empty() {
                skipped += 1;
                continue;
            }

            // 切分的音频文件放到临时目录
            let tmp_audio = tmp_dir.join(format!("{}.wav", entry.index));
            // 合成的音频文件放到输出目录
            let output_path = output_dir.join(format!("{}.wav", entry.index));

            tasks.push(Task {
                entry,
                tmp_audio,
                output_path,
            });
        }

        if skipped > 0 {
            println!("跳过 {} 个空文本条目", skipped);
        }

        Ok(Self { tasks })
    }

    pub async fn prepare_audio_segments(
        &self,
        audio_path: &Path,
        max_concurrent: usize,
    ) -> Result<()> {
        // 使用信号量限制并发数，避免内存和CPU过载
        let semaphore = Arc::new(Semaphore::new(max_concurrent));
        let progress = Arc::new(ProgressBar::new(self.tasks.len() as u64));
        progress.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} 切分音频 ({eta})")
                .unwrap()
                .progress_chars("#>-"),
        );

        // 使用并发控制切分音频片段
        let futures: Vec<_> = self
            .tasks
            .iter()
            .map(|task| {
                let audio_path = audio_path.to_path_buf();
                let tmp_audio = task.tmp_audio.clone();
                let start = task.entry.start_time;
                let end = task.entry.end_time;
                let index = task.entry.index;
                let semaphore = semaphore.clone();
                let progress = progress.clone();

                async move {
                    // 获取并发许可
                    let _permit = semaphore.acquire().await.unwrap();

                    let result = async {
                        let output_dir = tmp_audio.parent().unwrap();

                        // 切分音频片段到临时文件
                        let temp_path =
                            AudioSplitter::split_audio(&audio_path, start, end, output_dir, index)
                                .await?;

                        // 验证切分后的文件大小
                        let temp_size = tokio::fs::metadata(&temp_path).await?.len();
                        if temp_size <= 44 {
                            // 如果文件太小，记录警告但继续处理（会在后续任务执行时被跳过）
                            eprintln!(
                                "警告: 任务 {} 的音频片段可能无效（文件大小: {} 字节）",
                                index, temp_size
                            );
                        }

                        // 复制到 speaker 和 emotion 路径（使用相同的音频片段）
                        tokio::fs::copy(&temp_path, &tmp_audio).await?;

                        // 删除临时切分文件（speaker 和 emotion 文件保留在 tmp 目录，任务完成后统一删除）
                        let _ = tokio::fs::remove_file(&temp_path).await;

                        Ok::<(), anyhow::Error>(())
                    }
                    .await;

                    progress.inc(1);
                    result
                }
            })
            .collect();

        futures::future::try_join_all(futures).await?;
        progress.finish_with_message("音频切分完成");
        Ok(())
    }

    pub fn get_tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }
}

/// 根据错误类型决定退避时长：429/5xx 更长，4xx 快速失败，默认指数退避
fn classify_backoff_delay(err: &anyhow::Error, attempt: u32) -> u64 {
    let msg = err.to_string();
    // 429 或 5xx: 更长退避
    if msg.contains("429") || msg.contains(" 5") {
        // 2s, 4s, 8s, 上限 30s
        let base = 2000u64 * (1 << (attempt - 1).min(4));
        return base.min(30_000);
    }

    // 4xx（除 429）直接短退避，避免无意义重试
    if msg.contains(" 4") {
        return 500;
    }

    // 默认指数退避：1秒、2秒、4秒、8秒、16秒、32秒（最多）
    let delay_ms = 1000 * (1 << (attempt - 1).min(5));
    delay_ms
}
