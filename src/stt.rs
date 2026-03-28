use reqwest::multipart::{Form, Part};
use serde::Deserialize;

use crate::{
    config::{Config, SttProvider},
    error::{AppError, AppResult},
};

const ELEVENLABS_STT_URL: &str = "https://api.elevenlabs.io/v1/speech-to-text";
const ELEVENLABS_MODEL_ID: &str = "scribe_v2";

#[derive(Clone)]
pub enum SttClient {
    Disabled,
    ElevenLabs(ElevenLabsSttClient),
}

impl SttClient {
    pub fn from_config(config: &Config) -> AppResult<Self> {
        match config.stt_provider {
            SttProvider::None => Ok(Self::Disabled),
            SttProvider::ElevenLabs => {
                let api_key = config.stt_api_key.clone().ok_or_else(|| {
                    AppError::Config("ElevenLabs API key is required when STT is enabled".into())
                })?;
                Ok(Self::ElevenLabs(ElevenLabsSttClient::new(
                    reqwest::Client::new(),
                    api_key,
                )))
            }
        }
    }

    pub async fn transcribe_voice(
        &self,
        file_name: &str,
        mime_type: &str,
        audio_bytes: Vec<u8>,
    ) -> AppResult<String> {
        match self {
            Self::Disabled => Err(AppError::Validation(
                "voice input is unavailable until Atlas2 is started with --stt-provider 11labs"
                    .into(),
            )),
            Self::ElevenLabs(client) => {
                client
                    .transcribe_voice(file_name, mime_type, audio_bytes)
                    .await
            }
        }
    }
}

#[derive(Clone)]
pub struct ElevenLabsSttClient {
    http: reqwest::Client,
    api_key: String,
}

impl ElevenLabsSttClient {
    pub fn new(http: reqwest::Client, api_key: String) -> Self {
        Self { http, api_key }
    }

    pub async fn transcribe_voice(
        &self,
        file_name: &str,
        mime_type: &str,
        audio_bytes: Vec<u8>,
    ) -> AppResult<String> {
        let part = Part::bytes(audio_bytes)
            .file_name(file_name.to_string())
            .mime_str(mime_type)
            .map_err(|error| AppError::Validation(format!("invalid voice MIME type: {error}")))?;
        let form = Form::new()
            .text("model_id", ELEVENLABS_MODEL_ID.to_string())
            .part("file", part);

        let response = self
            .http
            .post(ELEVENLABS_STT_URL)
            .header("xi-api-key", &self.api_key)
            .multipart(form)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "unable to read error body".into());
            let detail = summarize_stt_error_body(&body);
            return Err(AppError::Validation(format!(
                "ElevenLabs transcription failed with status {status}: {detail}"
            )));
        }

        let payload: ElevenLabsTranscriptResponse = response.json().await?;
        let text = payload.text.trim().to_string();
        if text.is_empty() {
            return Err(AppError::Validation(
                "voice message transcription was empty".into(),
            ));
        }

        Ok(text)
    }
}

#[derive(Debug, Deserialize)]
struct ElevenLabsTranscriptResponse {
    text: String,
}

fn summarize_stt_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "no additional details".into();
    }

    if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(detail) = json
            .get("detail")
            .and_then(serde_json::Value::as_str)
            .or_else(|| json.get("message").and_then(serde_json::Value::as_str))
        {
            return detail.to_string();
        }
    }

    let mut compact = trimmed.replace('\n', " ");
    if compact.len() > 200 {
        compact.truncate(200);
        compact.push_str("...");
    }
    compact
}

#[cfg(test)]
mod tests {
    use super::{ElevenLabsTranscriptResponse, SttClient, summarize_stt_error_body};

    #[test]
    fn disabled_stt_rejects_voice_messages() {
        let error = futures::executor::block_on(async {
            SttClient::Disabled
                .transcribe_voice("voice.oga", "audio/ogg", Vec::new())
                .await
        })
        .unwrap_err();

        assert!(error.to_string().contains("--stt-provider 11labs"));
    }

    #[test]
    fn extracts_detail_from_json_error_body() {
        let body = r#"{"detail":"invalid api key"}"#;
        assert_eq!(summarize_stt_error_body(body), "invalid api key");
    }

    #[test]
    fn transcript_payload_deserializes() {
        let payload: ElevenLabsTranscriptResponse =
            serde_json::from_str(r#"{"text":"hello from voice"}"#).unwrap();
        assert_eq!(payload.text, "hello from voice");
    }
}
