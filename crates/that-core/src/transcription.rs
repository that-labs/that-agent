use anyhow::{Context, Result};

/// Transcribe audio bytes using the OpenAI Whisper API.
pub async fn transcribe(api_key: &str, data: &[u8], mime_type: &str) -> Result<String> {
    let ext = mime_type.split('/').nth(1).unwrap_or("ogg");
    let filename = format!("audio.{ext}");

    let part = reqwest::multipart::Part::bytes(data.to_vec())
        .file_name(filename)
        .mime_str(mime_type)?;

    let form = reqwest::multipart::Form::new()
        .text("model", "whisper-1")
        .part("file", part);

    let resp = reqwest::Client::new()
        .post("https://api.openai.com/v1/audio/transcriptions")
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .context("whisper request failed")?;

    let json: serde_json::Value = resp.json().await.context("whisper response parse")?;
    json["text"]
        .as_str()
        .map(|s| s.to_string())
        .context("no text field in whisper response")
}
