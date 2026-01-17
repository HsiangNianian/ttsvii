use crate::task::{TaskExecutor, TaskManager};
use crate::{api, audio, srt};
use anyhow::{Context, Result};
use axum::{
    extract::{ws::WebSocketUpgrade, Multipart, State},
    response::{Html, Json},
    routing::{get, post},
    Router,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::{broadcast, RwLock};
use tower_http::cors::CorsLayer;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskConfig {
    pub api_url: String,
    #[serde(default)]
    pub audio: String,
    #[serde(default)]
    pub srt: String,
    #[serde(default)]
    pub audio_filename: String,
    #[serde(default)]
    pub audio_content: String,
    #[serde(default)]
    pub srt_filename: String,
    #[serde(default)]
    pub srt_content: String,
    pub output: String,
    pub max_concurrent: usize,
    pub split_concurrent: usize,
    pub batch_size: usize,
    pub rest_duration: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskStatus {
    pub state: String, // "idle", "running", "paused", "completed", "error"
    pub progress: u64,
    pub total: u64,
    pub success: u64,
    pub failure: u64,
    pub skipped: u64,
    pub current_batch: usize,
    pub total_batches: usize,
    pub message: String,
    pub task_id: Option<String>,
    pub output_path: Option<String>,
}

#[derive(Clone)]
pub struct AppState {
    pub status: Arc<RwLock<TaskStatus>>,
    pub should_stop: Arc<AtomicBool>,
    pub task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<Result<()>>>>>,
    pub tx: broadcast::Sender<TaskStatus>,
}

impl AppState {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel::<TaskStatus>(100);
        Self {
            status: Arc::new(RwLock::new(TaskStatus {
                state: "idle".to_string(),
                progress: 0,
                total: 0,
                success: 0,
                failure: 0,
                skipped: 0,
                current_batch: 0,
                total_batches: 0,
                message: "就绪".to_string(),
                task_id: None,
                output_path: None,
            })),
            should_stop: Arc::new(AtomicBool::new(false)),
            task_handle: Arc::new(RwLock::new(None)),
            tx,
        }
    }

    pub async fn update_status(&self, status: TaskStatus) {
        *self.status.write().await = status.clone();
        let _ = self.tx.send(status);
    }

    pub async fn start_task(&self, config: TaskConfig) -> Result<()> {
        // 检查是否已有任务在运行
        if let Some(handle) = self.task_handle.read().await.as_ref() {
            if !handle.is_finished() {
                anyhow::bail!("已有任务正在运行");
            }
        }

        self.should_stop.store(false, Ordering::Relaxed);
        let state = self.clone();

        let handle = tokio::spawn(async move {
            state
                .update_status(TaskStatus {
                    state: "running".to_string(),
                    progress: 0,
                    total: 0,
                    success: 0,
                    failure: 0,
                    skipped: 0,
                    current_batch: 0,
                    total_batches: 0,
                    message: "正在初始化...".to_string(),
                    task_id: None,
                    output_path: None,
                })
                .await;

            let result = async {
                // 检查 ffmpeg
                audio::AudioSplitter::check_ffmpeg()
                    .await
                    .context("请确保已安装 ffmpeg 并在 PATH 中")?;

                // 生成本次运行的 UUID
                let task_uuid = Uuid::new_v4();
                let uuid_str = task_uuid.to_string();

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: 0,
                        total: 0,
                        success: 0,
                        failure: 0,
                        skipped: 0,
                        current_batch: 0,
                        total_batches: 0,
                        message: format!("任务 ID: {}", uuid_str),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                // 创建输出目录
                let output_path = PathBuf::from(&config.output);
                tokio::fs::create_dir_all(&output_path).await?;

                // 创建临时目录
                let tmp_dir = std::env::current_dir()?.join("tmp").join(&uuid_str);
                tokio::fs::create_dir_all(&tmp_dir).await?;

                // 创建输出目录
                let output_task_dir = output_path.join(&uuid_str);
                tokio::fs::create_dir_all(&output_task_dir).await?;

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: 0,
                        total: 0,
                        success: 0,
                        failure: 0,
                        skipped: 0,
                        current_batch: 0,
                        total_batches: 0,
                        message: "正在解析 SRT 文件...".to_string(),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                let srt_entries = srt::SrtParser::parse_file(&PathBuf::from(&config.srt))
                    .context("解析 SRT 文件失败")?;

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: 0,
                        total: srt_entries.len() as u64,
                        success: 0,
                        failure: 0,
                        skipped: 0,
                        current_batch: 0,
                        total_batches: 0,
                        message: format!("找到 {} 个字幕条目", srt_entries.len()),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                // 创建任务管理器
                let task_manager = TaskManager::new(
                    srt_entries,
                    &PathBuf::from(&config.audio),
                    &tmp_dir,
                    &output_task_dir,
                )?;

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: 0,
                        total: task_manager.len() as u64,
                        success: 0,
                        failure: 0,
                        skipped: 0,
                        current_batch: 0,
                        total_batches: 0,
                        message: format!(
                            "正在切分音频文件（并发数: {}）...",
                            config.split_concurrent
                        ),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                task_manager
                    .prepare_audio_segments(&PathBuf::from(&config.audio), config.split_concurrent)
                    .await?;

                // 创建 API 客户端
                let api_client = Arc::new(api::ApiClient::new(config.api_url.clone()));

                // 创建任务执行器
                let executor = Arc::new(TaskExecutor::new(api_client, config.max_concurrent));
                executor.set_total(task_manager.len() as u64);

                let tasks = task_manager.get_tasks();
                let tasks_len = tasks.len();
                let batch_task_count = config.batch_size * config.max_concurrent;
                let total_batches = (tasks_len + batch_task_count - 1) / batch_task_count;

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: 0,
                        total: tasks_len as u64,
                        success: 0,
                        failure: 0,
                        skipped: 0,
                        current_batch: 0,
                        total_batches,
                        message: format!(
                            "开始处理 {} 个任务（并发数: {}, 批次大小: {}, 休息时间: {}s）...",
                            tasks_len,
                            config.max_concurrent,
                            config.batch_size,
                            config.rest_duration
                        ),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                let mut all_results = Vec::new();
                let mut batch_num = 0;

                for batch_start in (0..tasks_len).step_by(batch_task_count) {
                    // 检查是否需要停止
                    if state.should_stop.load(Ordering::Relaxed) {
                        state
                            .update_status(TaskStatus {
                                state: "paused".to_string(),
                                progress: all_results.len() as u64,
                                total: tasks_len as u64,
                                success: 0,
                                failure: 0,
                                skipped: 0,
                                current_batch: batch_num,
                                total_batches,
                                message: "任务已暂停".to_string(),
                                task_id: Some(uuid_str.clone()),
                                output_path: None,
                            })
                            .await;
                        anyhow::bail!("任务已停止");
                    }

                    batch_num += 1;
                    let batch_end = (batch_start + batch_task_count).min(tasks_len);
                    let batch_tasks = &tasks[batch_start..batch_end];

                    state
                        .update_status(TaskStatus {
                            state: "running".to_string(),
                            progress: all_results.len() as u64,
                            total: tasks_len as u64,
                            success: 0,
                            failure: 0,
                            skipped: 0,
                            current_batch: batch_num,
                            total_batches,
                            message: format!(
                                "批次 {}/{}: 处理任务 {}-{}",
                                batch_num,
                                total_batches,
                                batch_start + 1,
                                batch_end
                            ),
                            task_id: Some(uuid_str.clone()),
                            output_path: None,
                        })
                        .await;

                    // 执行当前批次
                    let futures: Vec<_> = batch_tasks
                        .iter()
                        .map(|task| {
                            let task = task.clone();
                            let executor = executor.clone();
                            async move { executor.execute_task(task).await }
                        })
                        .collect();

                    let batch_results = futures::future::join_all(futures).await;
                    all_results.extend(batch_results);

                    // 更新统计
                    let (success, failure) = executor.get_stats();
                    let completed = success + failure;

                    state
                        .update_status(TaskStatus {
                            state: "running".to_string(),
                            progress: completed,
                            total: tasks_len as u64,
                            success,
                            failure,
                            skipped: 0,
                            current_batch: batch_num,
                            total_batches,
                            message: format!(
                                "实时统计: 进度 {}/{} | 成功: {} | 失败: {}",
                                completed, tasks_len, success, failure
                            ),
                            task_id: Some(uuid_str.clone()),
                            output_path: None,
                        })
                        .await;

                    // 如果不是最后一批，休息一下
                    if batch_end < tasks_len {
                        state
                            .update_status(TaskStatus {
                                state: "running".to_string(),
                                progress: completed,
                                total: tasks_len as u64,
                                success,
                                failure,
                                skipped: 0,
                                current_batch: batch_num,
                                total_batches,
                                message: format!("休息 {} 秒...", config.rest_duration),
                                task_id: Some(uuid_str.clone()),
                                output_path: None,
                            })
                            .await;
                        tokio::time::sleep(std::time::Duration::from_secs(config.rest_duration))
                            .await;
                    }
                }

                let results = all_results;

                // 检查结果
                let mut errors = Vec::new();
                let mut skipped = Vec::new();
                for (i, result) in results.into_iter().enumerate() {
                    if let Err(e) = result {
                        let err_msg = e.to_string();
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

                let success_count = tasks_len - errors.len() - skipped.len();

                if !skipped.is_empty() {
                    state
                        .update_status(TaskStatus {
                            state: "running".to_string(),
                            progress: tasks_len as u64,
                            total: tasks_len as u64,
                            success: success_count as u64,
                            failure: errors.len() as u64,
                            skipped: skipped.len() as u64,
                            current_batch: total_batches,
                            total_batches,
                            message: format!("跳过 {} 个无效任务", skipped.len()),
                            task_id: Some(uuid_str.clone()),
                            output_path: None,
                        })
                        .await;
                }

                if !errors.is_empty() {
                    state
                        .update_status(TaskStatus {
                            state: "error".to_string(),
                            progress: tasks_len as u64,
                            total: tasks_len as u64,
                            success: success_count as u64,
                            failure: errors.len() as u64,
                            skipped: skipped.len() as u64,
                            current_batch: total_batches,
                            total_batches,
                            message: format!("有 {} 个任务失败", errors.len()),
                            task_id: Some(uuid_str.clone()),
                            output_path: None,
                        })
                        .await;
                    anyhow::bail!("部分任务执行失败");
                }

                state
                    .update_status(TaskStatus {
                        state: "running".to_string(),
                        progress: tasks_len as u64,
                        total: tasks_len as u64,
                        success: success_count as u64,
                        failure: 0,
                        skipped: skipped.len() as u64,
                        current_batch: total_batches,
                        total_batches,
                        message: "所有任务执行完成，正在合并音频...".to_string(),
                        task_id: Some(uuid_str.clone()),
                        output_path: None,
                    })
                    .await;

                // 收集所有合成的音频文件
                let mut audio_files = Vec::new();
                for task in tasks.iter() {
                    if task.output_path.exists() {
                        let metadata = tokio::fs::metadata(&task.output_path).await?;
                        if metadata.len() > 0 {
                            audio_files.push(task.output_path.clone());
                        }
                    }
                }

                if audio_files.is_empty() {
                    anyhow::bail!("没有有效的合成音频文件可以合并");
                }

                audio_files.sort();

                // 合并音频
                let final_output = output_path.join(format!("{}.wav", uuid_str));
                audio::AudioSplitter::merge_audio(&audio_files, &final_output).await?;

                state
                    .update_status(TaskStatus {
                        state: "completed".to_string(),
                        progress: tasks_len as u64,
                        total: tasks_len as u64,
                        success: success_count as u64,
                        failure: 0,
                        skipped: skipped.len() as u64,
                        current_batch: total_batches,
                        total_batches,
                        message: format!("完成！最终音频已保存到: {}", final_output.display()),
                        task_id: Some(uuid_str.clone()),
                        output_path: Some(final_output.display().to_string()),
                    })
                    .await;

                Ok::<(), anyhow::Error>(())
            }
            .await;

            if let Err(ref e) = result {
                let current_status = state.status.read().await.clone();
                let error_msg = e.to_string();
                state
                    .update_status(TaskStatus {
                        state: "error".to_string(),
                        progress: current_status.progress,
                        total: current_status.total,
                        success: current_status.success,
                        failure: current_status.failure,
                        skipped: current_status.skipped,
                        current_batch: current_status.current_batch,
                        total_batches: current_status.total_batches,
                        message: format!("错误: {}", error_msg),
                        task_id: current_status.task_id,
                        output_path: current_status.output_path,
                    })
                    .await;
            }

            result
        });

        *self.task_handle.write().await = Some(handle);
        Ok(())
    }

    pub async fn stop_task(&self) {
        self.should_stop.store(true, Ordering::Relaxed);
    }
}

pub async fn create_router() -> Router {
    let state = AppState::new();

    Router::new()
        .route("/", get(index_handler))
        .route("/api/status", get(get_status))
        .route("/api/start", post(start_task))
        .route("/api/stop", post(stop_task))
        .route("/api/upload/chunk", post(upload_chunk))
        .route("/api/upload/check", get(check_upload))
        .route("/api/upload/merge", post(merge_upload))
        .route("/ws", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../webui/index.html"))
}

async fn get_status(State(state): State<AppState>) -> Json<TaskStatus> {
    Json(state.status.read().await.clone())
}

async fn start_task(
    State(state): State<AppState>,
    Json(config): Json<TaskConfig>,
) -> Json<serde_json::Value> {
    // 验证文件路径
    let audio_path = std::path::Path::new(&config.audio);
    if !audio_path.exists() {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("音频文件不存在: {}", config.audio)
        }));
    }

    let srt_path = std::path::Path::new(&config.srt);
    if !srt_path.exists() {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("SRT 字幕文件不存在: {}", config.srt)
        }));
    }

    match state.start_task(config).await {
        Ok(_) => Json(serde_json::json!({ "success": true })),
        Err(e) => Json(serde_json::json!({ "success": false, "error": e.to_string() })),
    }
}

// 上传分块
async fn upload_chunk(mut multipart: Multipart) -> Json<serde_json::Value> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    
    // 存储上传信息（实际应用中应该使用数据库或文件系统）
    static UPLOAD_INFO: OnceLock<Mutex<HashMap<String, Vec<usize>>>> = OnceLock::new();
    let upload_info = UPLOAD_INFO.get_or_init(|| Mutex::new(HashMap::new()));

    let temp_dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("tmp")
        .join("uploads");

    if let Err(e) = tokio::fs::create_dir_all(&temp_dir).await {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("创建临时目录失败: {}", e)
        }));
    }

    let mut upload_id = String::new();
    let mut file_type = String::new();
    let mut chunk_index = 0usize;
    let mut chunk_data = Vec::new();

    // 解析 multipart 数据
    while let Some(field) = multipart.next_field().await.unwrap_or_else(|e| {
        eprintln!("解析 multipart 字段失败: {}", e);
        None
    }) {
        let name = field.name().unwrap_or("").to_string();

        if name == "chunk" {
            if let Ok(data) = field.bytes().await {
                chunk_data = data.to_vec();
            }
        } else if name == "upload_id" {
            if let Ok(text) = field.text().await {
                upload_id = text;
            }
        } else if name == "file_type" {
            if let Ok(text) = field.text().await {
                file_type = text;
            }
        } else if name == "chunk_index" {
            if let Ok(text) = field.text().await {
                chunk_index = text.parse().unwrap_or(0);
            }
        }
    }

    if upload_id.is_empty() || file_type.is_empty() || chunk_data.is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "error": "缺少必要参数"
        }));
    }

    // 保存分块
    let chunk_dir = temp_dir.join(&upload_id);
    if let Err(e) = tokio::fs::create_dir_all(&chunk_dir).await {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("创建分块目录失败: {}", e)
        }));
    }

    let chunk_path = chunk_dir.join(format!("chunk_{}", chunk_index));
    if let Err(e) = tokio::fs::write(&chunk_path, &chunk_data).await {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("保存分块失败: {}", e)
        }));
    }

    // 记录已上传的分块
    let key = format!("{}_{}", upload_id, file_type);
    let mut info = upload_info.lock().unwrap();
    let chunks = info.entry(key.clone()).or_insert_with(Vec::new);
    if !chunks.contains(&chunk_index) {
        chunks.push(chunk_index);
    }

    Json(serde_json::json!({
        "success": true,
        "chunk_index": chunk_index
    }))
}

// 检查已上传的分块
async fn check_upload(
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Json<serde_json::Value> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    
    static UPLOAD_INFO: OnceLock<Mutex<HashMap<String, Vec<usize>>>> = OnceLock::new();
    let upload_info = UPLOAD_INFO.get_or_init(|| Mutex::new(HashMap::new()));
    
    let upload_id = params.get("upload_id").cloned().unwrap_or_default();
    let file_type = params.get("file_type").cloned().unwrap_or_default();

    if upload_id.is_empty() || file_type.is_empty() {
        return Json(serde_json::json!({
            "success": false,
            "error": "缺少必要参数"
        }));
    }

    let key = format!("{}_{}", upload_id, file_type);
    let info = upload_info.lock().unwrap();
    let uploaded_chunks = info.get(&key).cloned().unwrap_or_default();

    Json(serde_json::json!({
        "success": true,
        "uploaded_chunks": uploaded_chunks
    }))
}

// 合并文件
async fn merge_upload(Json(params): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let upload_id = params
        .get("upload_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let file_type = params
        .get("file_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let filename = params
        .get("filename")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let total_chunks = params
        .get("total_chunks")
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as usize;

    if upload_id.is_empty() || file_type.is_empty() || total_chunks == 0 {
        return Json(serde_json::json!({
            "success": false,
            "error": "缺少必要参数"
        }));
    }

    let temp_dir = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("tmp")
        .join("uploads");

    let chunk_dir = temp_dir.join(upload_id);
    let unique_id = Uuid::new_v4().to_string();
    let extension = std::path::Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or(if file_type == "audio" { "mp3" } else { "srt" });

    let final_filename = format!("{}_{}.{}", file_type, unique_id, extension);
    let final_path = temp_dir.join(&final_filename);

    // 合并所有分块
    let mut output_file = match tokio::fs::File::create(&final_path).await {
        Ok(file) => file,
        Err(e) => {
            return Json(serde_json::json!({
                "success": false,
                "error": format!("创建文件失败: {}", e)
            }));
        }
    };

    use tokio::io::AsyncWriteExt;
    for chunk_index in 0..total_chunks {
        let chunk_path = chunk_dir.join(format!("chunk_{}", chunk_index));
        if !chunk_path.exists() {
            return Json(serde_json::json!({
                "success": false,
                "error": format!("分块 {} 不存在", chunk_index)
            }));
        }

        let chunk_data = match tokio::fs::read(&chunk_path).await {
            Ok(data) => data,
            Err(e) => {
                return Json(serde_json::json!({
                    "success": false,
                    "error": format!("读取分块 {} 失败: {}", chunk_index, e)
                }));
            }
        };

        if let Err(e) = output_file.write_all(&chunk_data).await {
            return Json(serde_json::json!({
                "success": false,
                "error": format!("写入分块 {} 失败: {}", chunk_index, e)
            }));
        }
    }

    if let Err(e) = output_file.sync_all().await {
        return Json(serde_json::json!({
            "success": false,
            "error": format!("同步文件失败: {}", e)
        }));
    }

    // 清理分块目录
    let _ = tokio::fs::remove_dir_all(&chunk_dir).await;

    Json(serde_json::json!({
        "success": true,
        "file_path": final_path.to_string_lossy().to_string()
    }))
}

async fn stop_task(State(state): State<AppState>) -> Json<serde_json::Value> {
    state.stop_task().await;
    Json(serde_json::json!({ "success": true }))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> axum::response::Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, state: AppState) {
    use futures_util::{SinkExt, StreamExt};

    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

    // 发送当前状态
    let current_status = state.status.read().await.clone();
    let _ = sender
        .send(axum::extract::ws::Message::Text(
            serde_json::to_string(&current_status).unwrap(),
        ))
        .await;

    // 监听状态更新
    let mut send_task = tokio::spawn(async move {
        while let Ok(status) = rx.recv().await {
            let msg = axum::extract::ws::Message::Text(serde_json::to_string(&status).unwrap());
            if sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 监听客户端消息（用于保持连接）
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(_)) = receiver.next().await {
            // 忽略客户端消息，仅用于保持连接
        }
    });

    tokio::select! {
        _ = &mut send_task => {},
        _ = &mut recv_task => {},
    }
}
