use anyhow::{Context, Result};
use reqwest::multipart;
use std::path::Path;

pub struct ApiClient {
    client: reqwest::Client,
    base_url: String,
}

impl ApiClient {
    pub fn new(base_url: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .expect("创建 HTTP 客户端失败");

        Self { client, base_url }
    }

    pub async fn synthesize(
        &self,
        text: &str,
        speaker_audio: Option<&Path>,
        emotion_audio: Option<&Path>,
        prompt_audio: Option<&str>,
        duration_factor: Option<f64>,
        target_duration: Option<f64>,
    ) -> Result<Vec<u8>> {
        let url = format!("{}/synthesize", self.base_url.trim_end_matches('/'));

        let mut form = multipart::Form::new().text("text", text.to_string());

        if let Some(duration_factor) = duration_factor {
            form = form.text("duration_factor", duration_factor.to_string());
        }

        if let Some(target_duration) = target_duration {
            form = form.text("target_duration", target_duration.to_string());
        }

        if let Some(prompt_audio) = prompt_audio {
            form = form.text("prompt_audio", prompt_audio.to_string());
        }

        if let Some(speaker_path) = speaker_audio {
            let file_name = speaker_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("speaker_audio.wav");

            let file_content = tokio::fs::read(speaker_path)
                .await
                .with_context(|| format!("无法读取音频文件: {:?}", speaker_path))?;

            let part = multipart::Part::bytes(file_content)
                .file_name(file_name.to_string())
                .mime_str("audio/wav")
                .context("创建 multipart part 失败")?;

            form = form.part("speaker_audio", part);
        }

        if let Some(emotion_path) = emotion_audio {
            let file_name = emotion_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("emotion_audio.wav");

            let file_content = tokio::fs::read(emotion_path)
                .await
                .with_context(|| format!("无法读取音频文件: {:?}", emotion_path))?;

            let part = multipart::Part::bytes(file_content)
                .file_name(file_name.to_string())
                .mime_str("audio/wav")
                .context("创建 multipart part 失败")?;

            form = form.part("emotion_audio", part);
        }

        let response = self
            .client
            .post(&url)
            .multipart(form)
            .send()
            .await
            .context("发送 API 请求失败")?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            anyhow::bail!("API 请求失败: {} - {}", status, text);
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|h| h.to_str().ok())
            .unwrap_or("");

        // 检查响应类型
        if content_type.contains("application/json") {
            // 尝试解析 JSON 响应
            let json: serde_json::Value = response.json().await.context("解析 JSON 响应失败")?;

            // 如果 JSON 中包含音频 URL，则下载
            if let Some(url_str) = json.get("url").and_then(|v| v.as_str()) {
                let audio_response = self
                    .client
                    .get(url_str)
                    .send()
                    .await
                    .context("下载音频文件失败")?;

                let audio_data = audio_response.bytes().await.context("读取音频数据失败")?;

                return Ok(audio_data.to_vec());
            }

            // 如果 JSON 中包含 base64 编码的音频数据
            if let Some(base64_str) = json.get("audio").and_then(|v| v.as_str()) {
                use base64::Engine;
                let audio_data = base64::engine::general_purpose::STANDARD
                    .decode(base64_str)
                    .context("解码 base64 音频数据失败")?;
                return Ok(audio_data);
            }

            anyhow::bail!("JSON 响应中未找到音频数据: {}", json);
        } else {
            // 直接返回二进制数据（音频文件）
            let audio_data = response.bytes().await.context("读取响应数据失败")?;

            Ok(audio_data.to_vec())
        }
    }
}
