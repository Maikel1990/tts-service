#![warn(clippy::pedantic)]
#![allow(
    clippy::unused_async,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_lossless
)]

use std::{
    fmt::{Display, Write as _},
    str::FromStr,
    sync::OnceLock,
};

use axum::{http::header::HeaderValue, response::Response};
use bytes::Bytes;
use deadpool_redis::redis::AsyncCommands;
use serde_json::to_value;
use sha2::Digest;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod espeak;
mod gcloud;
mod gtts;
mod polly;

type Result<T, E = anyhow::Error> = std::result::Result<T, E>;
type ResponseResult<T> = std::result::Result<T, Error>;

#[must_use]
pub fn check_mp3_length(audio: &[u8], max_length: u64) -> bool {
    use bytes::Buf;
    mp3_duration::from_read(&mut audio.reader()).map_or(true, |d| d.as_secs() < max_length)
}

#[derive(serde::Deserialize)]
struct GetVoices {
    mode: TTSMode,
    #[serde(default)]
    raw: bool,
}

async fn get_voices(
    axum::extract::Query(payload): axum::extract::Query<GetVoices>,
) -> ResponseResult<impl axum::response::IntoResponse> {
    let GetVoices { mode, raw } = payload;
    let state = STATE.get().unwrap();

    Ok(axum::Json(if raw {
        match mode {
            TTSMode::gTTS => to_value(gtts::get_raw_voices()),
            TTSMode::eSpeak => to_value(espeak::get_voices()),
            TTSMode::Polly => to_value(polly::get_raw_voices(&state.polly).await?),
            TTSMode::gCloud => to_value(gcloud::get_raw_voices(&state.gcloud).await?),
        }?
    } else {
        to_value(match mode {
            TTSMode::gTTS => gtts::get_voices(),
            TTSMode::eSpeak => espeak::get_voices().to_vec(),
            TTSMode::Polly => polly::get_voices(&state.polly).await?,
            TTSMode::gCloud => gcloud::get_voices(&state.gcloud).await?,
        })?
    }))
}

#[derive(serde::Deserialize)]
struct GetTTS {
    text: String,
    mode: TTSMode,
    #[serde(rename = "lang")]
    voice: String,
    #[serde(default)]
    speaking_rate: Option<f32>,
    max_length: Option<u64>,
    #[serde(default)]
    preferred_format: Option<String>,
}

async fn get_tts(
    axum::extract::Query(payload): axum::extract::Query<GetTTS>,
    headers: axum::http::HeaderMap,
) -> ResponseResult<Response<axum::body::Body>> {
    let state = STATE.get().unwrap();
    if let Some(auth_key) = state.auth_key.as_deref() {
        if headers
            .get("Authorization")
            .map(HeaderValue::to_str)
            .transpose()?
            != Some(auth_key)
        {
            return Err(Error::Unauthorized);
        }
    }

    let preferred_format = payload.preferred_format;
    let speaking_rate = payload.speaking_rate;
    let mut voice = payload.voice;
    let mode = payload.mode;
    let text = payload.text;

    mode.check_speaking_rate(speaking_rate)?;
    voice = mode.check_voice(state, voice).await?;

    let mut cache_key = format!(
        "{text} | {voice} | {mode} | {}",
        speaking_rate.unwrap_or(0.0)
    );

    if let Some(preferred_format) = preferred_format.as_ref() {
        write!(cache_key, "| {preferred_format}").unwrap();
    }

    tracing::debug!("Recieved request to TTS: {cache_key}");

    let redis_info = if let Some(redis_state) = &state.redis {
        let cache_hash = {
            let mut hasher = sha2::Sha256::new();
            hasher.update(&cache_key);
            hasher.finalize()
        };

        let mut conn = redis_state.client.get().await?;
        let cached_audio = conn
            .get::<_, Option<String>>(&*cache_hash)
            .await?
            .map(|enc| redis_state.key.decrypt(&enc))
            .transpose()?
            .map(Bytes::from);

        if let Some(cached_audio) = cached_audio {
            mode.check_length(&cached_audio, payload.max_length)?;

            tracing::debug!("Used cached TTS for {cache_key}");
            return mode.into_response(cached_audio, None);
        }

        Some((conn, &redis_state.key, cache_hash))
    } else {
        None
    };

    let (audio, content_type) = match mode {
        TTSMode::gTTS => gtts::get_tts(&state.gtts, &text, &voice).await?,
        TTSMode::eSpeak => {
            espeak::get_tts(&text, &voice, speaking_rate.map_or(0, |r| r as u16)).await?
        }
        TTSMode::Polly => {
            polly::get_tts(
                &state.polly,
                text,
                &voice,
                speaking_rate.map(|r| r as u8),
                preferred_format,
            )
            .await?
        }
        TTSMode::gCloud => {
            gcloud::get_tts(
                &state.gcloud,
                &text,
                &voice,
                speaking_rate.unwrap_or(0.0),
                preferred_format,
            )
            .await?
        }
    };

    tracing::debug!("Generated TTS from {cache_key}");
    if let Some((mut redis_conn, key, cache_hash)) = redis_info {
        if let Err(err) = redis_conn
            .set::<_, _, ()>(&*cache_hash, key.encrypt(&audio))
            .await
        {
            tracing::error!("Failed to cache: {err}");
        } else {
            tracing::debug!("Cached TTS from {cache_key}");
        };
    };

    mode.check_length(&audio, payload.max_length)?;
    mode.into_response(audio, content_type)
}

#[derive(serde::Deserialize, Clone, Copy, Debug)]
#[allow(non_camel_case_types)]
enum TTSMode {
    gTTS,
    Polly,
    eSpeak,
    gCloud,
}

impl TTSMode {
    #[allow(clippy::unused_self)]
    fn into_response(
        self,
        data: Bytes,
        _: Option<reqwest::header::HeaderValue>,
    ) -> ResponseResult<Response> {
        Response::builder()
            // TODO: Re-add when reqwest updates http to 1.0
            // .header(axum::http::header::CONTENT_TYPE, content_type.unwrap_or_else(|| HeaderValue::from_static(match self {
            //     #[cfg(feature="gtts")]    Self::gTTS    => "audio/mpeg",
            //     #[cfg(feature="espeak")]  Self::eSpeak  => "audio/wav",
            //     #[cfg(feature="gcloud")]  Self::gCloud  => "audio/opus",
            //     #[cfg(feature="polly")]   Self::Polly   => "audio/ogg",
            // })))
            .body(axum::body::Body::from(data))
            .map_err(Into::into)
    }

    #[cfg_attr(
        not(feature = "polly"),
        allow(unused_variables, clippy::unnecessary_wraps)
    )]
    async fn check_voice(self, state: &State, voice: String) -> ResponseResult<String> {
        if match self {
            Self::gTTS => gtts::check_voice(&voice),
            Self::eSpeak => espeak::check_voice(&voice),
            Self::gCloud => gcloud::check_voice(&state.gcloud, &voice).await?,
            Self::Polly => polly::check_voice(&state.polly, &voice).await?,
        } {
            Ok(voice)
        } else {
            Err(Error::UnknownVoice(voice))
        }
    }

    #[allow(unused_variables)]
    fn check_length(self, audio: &[u8], max_length: Option<u64>) -> ResponseResult<()> {
        if max_length.map_or(true, |max_length| match self {
            Self::gTTS => check_mp3_length(audio, max_length),
            Self::eSpeak => espeak::check_length(audio, max_length as u32),
            Self::gCloud | Self::Polly => true,
        }) {
            Ok(())
        } else {
            Err(Error::AudioTooLong)
        }
    }

    fn check_speaking_rate(self, speaking_rate: Option<f32>) -> ResponseResult<()> {
        if let Some(speaking_rate) = speaking_rate {
            if let Some(max) = self.max_speaking_rate() {
                if speaking_rate > max {
                    return Err(Error::InvalidSpeakingRate(speaking_rate));
                }
            }
        }

        Ok(())
    }

    #[allow(clippy::unnecessary_wraps)]
    const fn max_speaking_rate(self) -> Option<f32> {
        match self {
            Self::gTTS => None,
            Self::Polly => Some(500.0),
            Self::eSpeak => Some(400.0),
            Self::gCloud => Some(4.0),
        }
    }
}

impl Display for TTSMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::gTTS => "gTTS",
            Self::Polly => "Polly",
            Self::eSpeak => "eSpeak",
            Self::gCloud => "gCloud",
        })
    }
}

struct RedisCache {
    client: deadpool_redis::Pool,
    key: fernet::Fernet,
}

struct State {
    auth_key: Option<String>,
    redis: Option<RedisCache>,
    polly: polly::State,
    gtts: tokio::sync::RwLock<gtts::State>,
    gcloud: tokio::sync::RwLock<gcloud::State>,
}

static STATE: OnceLock<State> = OnceLock::new();

#[tokio::main]
async fn main() -> Result<()> {
    let fmt_layer = tracing_subscriber::fmt::layer();
    let filter = tracing_subscriber::filter::LevelFilter::from_str(
        &std::env::var("LOG_LEVEL").unwrap_or_else(|_| String::from("INFO")),
    )?;

    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(filter)
        .init();

    let redis_uri = std::env::var("REDIS_URI").ok();
    let result = STATE.set(State {
        gcloud: gcloud::State::new(reqwest::Client::new())?,
        gtts: tokio::sync::RwLock::new(gtts::get_random_ipv6().await?),
        polly: polly::State::new(&aws_config::load_from_env().await),

        auth_key: std::env::var("AUTH_KEY").ok(),
        redis: redis_uri.as_ref().map(|uri| {
            let key = std::env::var("CACHE_KEY").expect("CACHE_KEY not set!");
            RedisCache {
                client: deadpool_redis::Config::from_url(uri)
                    .create_pool(Some(deadpool_redis::Runtime::Tokio1))
                    .unwrap(),
                key: fernet::Fernet::new(&key).unwrap(),
            }
        }),
    });
    if result.is_err() {
        unreachable!()
    }

    let app = axum::Router::new()
        .route("/tts", axum::routing::get(get_tts))
        .route("/voices", axum::routing::get(get_voices))
        .route(
            "/modes",
            axum::routing::get(|| async {
                axum::Json([
                    TTSMode::gTTS.to_string(),
                    TTSMode::Polly.to_string(),
                    TTSMode::eSpeak.to_string(),
                    TTSMode::gCloud.to_string(),
                ])
            }),
        );

    let env_addr = std::env::var("BIND_ADDR");
    let bind_to = env_addr.as_deref().unwrap_or("0.0.0.0:3000");

    tracing::info!(
        "Binding to {bind_to} {} redis enabled!",
        if redis_uri.is_some() {
            "with"
        } else {
            "without"
        }
    );

    let listener = tokio::net::TcpListener::bind(bind_to).await?;
    axum::serve(listener, app.into_make_service()).await?;
    Ok(())
}

#[derive(Debug)]
enum Error {
    Unauthorized,
    UnknownVoice(String),
    AudioTooLong,
    InvalidSpeakingRate(f32),

    Unknown(anyhow::Error),
}

impl<E: Into<anyhow::Error>> From<E> for Error {
    fn from(e: E) -> Self {
        Self::Unknown(e.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSpeakingRate(rate) => write!(f, "Invalid speaking rate: {rate}"),
            Self::AudioTooLong => f.write_str("Max length exceeded!"),
            Self::UnknownVoice(voice) => write!(f, "Unknown voice: {voice}"),
            Self::Unauthorized => write!(f, "Unauthorized request"),
            Self::Unknown(e) => write!(f, "Unknown error: {e}"),
        }
    }
}

impl axum::response::IntoResponse for Error {
    fn into_response(self) -> Response {
        if let Error::Unknown(inner) = &self {
            tracing::error!("{inner:?}");
        };

        let json_err = serde_json::json!({
            "display": self.to_string(),
            "code": match self {
                Self::Unauthorized => 4,
                Self::InvalidSpeakingRate(_) => 3_u8,
                Self::AudioTooLong => 2,
                Self::UnknownVoice(_) => 1,
                Self::Unknown(_) => 0,
            },
        });

        let status = match self {
            Self::AudioTooLong | Self::InvalidSpeakingRate(_) => {
                axum::http::StatusCode::BAD_REQUEST
            }
            Self::Unknown(_) => axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Self::UnknownVoice(_) => axum::http::StatusCode::BAD_REQUEST,
            Self::Unauthorized => axum::http::StatusCode::FORBIDDEN,
        };

        (status, axum::Json(json_err)).into_response()
    }
}
