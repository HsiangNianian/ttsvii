use crate::api::ApiClient;
use crate::audio::AudioSplitter;
use crate::srt::SrtEntry;
use anyhow::Result;
use indicatif::{ProgressBar, ProgressStyle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct Task {
    pub entry: SrtEntry,
    pub speaker_audio: PathBuf,
    pub emotion_audio: PathBuf,
    pub output_path: PathBuf,
}

pub struct TaskExecutor {
    api_client: Arc<ApiClient>,
    semaphore: Arc<Semaphore>,
    progress: Arc<ProgressBar>,
}

impl TaskExecutor {
    pub fn new(api_client: Arc<ApiClient>, max_concurrent: usize) -> Self {
        let progress = ProgressBar::new(0);
        progress.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {pos}/{len} ({eta})")
                .unwrap()
                .progress_chars("#>-"),
        );

        Self {
            api_client,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
            progress: Arc::new(progress),
        }
    }

    pub async fn execute_task(&self, task: Task) -> Result<()> {
        let _permit = self.semaphore.acquire().await.unwrap();

        let result = self.execute_inner(&task).await;

        self.progress.inc(1);
        result
    }

    async fn execute_inner(&self, task: &Task) -> Result<()> {
        // 调用 API 合成音频
        let audio_data = self
            .api_client
            .synthesize(
                &task.entry.text,
                Some(&task.speaker_audio),
                Some(&task.emotion_audio),
                None,
                None,
                None,
            )
            .await?;

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

        for entry in srt_entries {
            // 切分的音频文件放到临时目录
            let speaker_audio = tmp_dir.join(format!("speaker_{}.wav", entry.index));
            let emotion_audio = tmp_dir.join(format!("emotion_{}.wav", entry.index));
            // 合成的音频文件放到输出目录
            let output_path = output_dir.join(format!("synthesized_{}.wav", entry.index));

            tasks.push(Task {
                entry,
                speaker_audio,
                emotion_audio,
                output_path,
            });
        }

        Ok(Self { tasks })
    }

    pub async fn prepare_audio_segments(&self, audio_path: &Path) -> Result<()> {
        // 并行切分所有音频片段
        let futures: Vec<_> = self
            .tasks
            .iter()
            .map(|task| {
                let audio_path = audio_path.to_path_buf();
                let speaker_path = task.speaker_audio.clone();
                let emotion_path = task.emotion_audio.clone();
                let start = task.entry.start_time;
                let end = task.entry.end_time;
                let index = task.entry.index;

                async move {
                    let output_dir = speaker_path.parent().unwrap();

                    // 直接切分音频片段到 speaker 路径
                    let temp_path =
                        AudioSplitter::split_audio(&audio_path, start, end, output_dir, index)
                            .await?;

                    // 复制到 emotion 路径（使用相同的音频片段）
                    tokio::fs::copy(&temp_path, &speaker_path).await?;
                    tokio::fs::copy(&temp_path, &emotion_path).await?;

                    // 删除临时文件
                    let _ = tokio::fs::remove_file(&temp_path).await;

                    Ok::<(), anyhow::Error>(())
                }
            })
            .collect();

        futures::future::try_join_all(futures).await?;
        Ok(())
    }

    pub fn get_tasks(&self) -> &[Task] {
        &self.tasks
    }

    pub fn len(&self) -> usize {
        self.tasks.len()
    }
}
