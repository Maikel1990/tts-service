#![warn(clippy::pedantic)]
#![allow(clippy::unused_async)]

#[cfg(not(any(feature="gtts", feature="espeak", feature="premium")))] 
compile_error!("Either feature `gtts`, `espeak`, or `premium` must be enabled!");

use std::{str::FromStr, sync::Arc, fmt::Display, borrow::Cow};

use sha2::Digest;
use redis::AsyncCommands;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[cfg(feature="gtts")] mod gtts;
#[cfg(feature="espeak")] mod espeak;
#[cfg(feature="premium")] mod premium;

type Result<T> = std::result::Result<T, anyhow::Error>;
type ResponseResult<T> = std::result::Result<T, Error>;

#[derive(serde::Deserialize)]
struct GetVoices {
    mode: TTSMode,
}

async fn get_voices(
    axum::extract::Query(payload): axum::extract::Query<GetVoices>
) -> ResponseResult<impl axum::response::IntoResponse> {
    let GetVoices{mode} = payload;

    let voices: Vec<String> = match mode {
        #[cfg(feature="gtts")] TTSMode::gTTS => gtts::get_voices(),
        #[cfg(feature="espeak")] TTSMode::eSpeak => espeak::get_voices(),
        #[cfg(feature="premium")] TTSMode::Premium => premium::get_voices(),
    };

    Ok(axum::Json(voices))
}


#[derive(serde::Deserialize)]
struct GetTTS {
    text: String,
    lang: String,
    mode: TTSMode,
    #[serde(default)] speaking_rate: f32
}

async fn get_tts(
    state: Arc<State>,
    axum::extract::Query(payload): axum::extract::Query<GetTTS>
) -> ResponseResult<impl axum::response::IntoResponse> {
    let GetTTS{text, lang, mode, speaking_rate} = payload;

    #[cfg(any(feature="premium", feature="espeak"))]
    mode.check_speaking_rate(speaking_rate)?;

    let cache_key = format!("{text} | {lang} | {mode} | {speaking_rate}");
    tracing::debug!("Recieved request to TTS: {cache_key}");

    let cache_hash = if state.redis.is_some() {
        let mut hasher = sha2::Sha256::new();
        hasher.update(cache_key.as_bytes());
        Some(hasher.finalize())
    } else {
        None
    };

    let mut redis = None;
    let cached_audio = if let Some(redis_state) = &state.redis {
        redis = Some((
            redis_state.client.get().await?,
            &redis_state.key
        ));

        let (conn, key) = redis.as_mut().unwrap();
        conn.get::<'_, _, Option<String>>(&*cache_hash.unwrap()).await?
            .map(|enc| key.decrypt(&enc))
            .transpose()?
            .map(bytes::Bytes::from)
    } else {
        None
    };

    let data: bytes::Bytes = match cached_audio {
        Some(cached_audio) => {
            tracing::debug!("Used cached TTS for {cache_key}");
            cached_audio
        }
        None => {
            let data = match mode {
                #[cfg(feature="gtts")] TTSMode::gTTS => gtts::get_tts(&state.gtts, &text, &lang).await?.bytes().await?,
                #[cfg(feature="espeak")] TTSMode::eSpeak => {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    bytes::Bytes::from(espeak::get_tts(&text, &lang, speaking_rate as u16).await?)
                },
                #[cfg(feature="premium")] TTSMode::Premium => {
                    bytes::Bytes::from(premium::get_tts(&state.premium, &text, &lang, speaking_rate).await?)
                }
            };

            tracing::debug!("Generated TTS from {cache_key}");
            if let Some((mut redis_conn, key)) = redis {
                if let Err(err) = redis_conn.set::<'_, _, _, ()>(&*cache_hash.unwrap(), key.encrypt(&data)).await {
                    tracing::error!("Failed to cache: {err}");
                } else {
                    tracing::debug!("Cached TTS from {cache_key}");
                };
            };

            data
        }
    };

    Ok(
        axum::response::Response::builder()
            .header("Content-Type", match mode {
                #[cfg(feature="gtts")] TTSMode::gTTS => "audio/mpeg",
                #[cfg(feature="espeak")] TTSMode::eSpeak => "audio/wav",
                #[cfg(feature="premium")] TTSMode::Premium => "audio/opus"
            })
            .body(axum::body::Full::new(data))?
    )
}


#[derive(serde::Deserialize, Clone, Copy, Debug)]
#[allow(non_camel_case_types)]
enum TTSMode {
    #[cfg(feature="gtts")] gTTS,
    #[cfg(feature="espeak")] eSpeak,
    #[cfg(feature="premium")] Premium,
}

#[cfg(any(feature="premium", feature="espeak"))]
impl TTSMode {
    fn check_speaking_rate(self, speaking_rate: f32) -> Result<()> {
        if let Some(max) = self.max_speaking_rate() {
            if speaking_rate > max {
                anyhow::bail!("Invalid speaking rate, it must be between 0 and {max}, as a 32-bit float")
            }
        }

        Ok(())
    }

    #[allow(clippy::unnecessary_wraps)]
    fn max_speaking_rate(self) -> Option<f32> {
        match self {
            #[cfg(feature="gtts")]    Self::gTTS    => None,
            #[cfg(feature="espeak")]  Self::eSpeak  => Some(400.0),
            #[cfg(feature="premium")] Self::Premium => Some(4.0),
        }
    }
}

impl Display for TTSMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            #[cfg(feature="gtts")] Self::gTTS => "gTTS",
            #[cfg(feature="espeak")] Self::eSpeak => "eSpeak",
            #[cfg(feature="premium")] Self::Premium => "Premium"
        })
    }
}


struct RedisCache {
    client: deadpool_redis::Pool,
    key: fernet::Fernet
}

struct State {
    redis: Option<RedisCache>,
    #[cfg(feature="gtts")] gtts: tokio::sync::RwLock<gtts::State>,
    #[cfg(feature="premium")] premium: tokio::sync::RwLock<premium::State>
}


#[tokio::main]
async fn main() -> Result<()> {
    let fmt_layer = tracing_subscriber::fmt::layer();
    let filter = tracing_subscriber::filter::LevelFilter::from_str(
        &std::env::var("LOG_LEVEL")
        .unwrap_or_else(|_| String::from("INFO"))
    )?;

    tracing_subscriber::registry().with(fmt_layer).with(filter).init();

    #[cfg(feature="espeak")] {
        // Init espeakng internally so we can fetch the voice path
        espeakng::initialise(None)?;
    }

    let state = Arc::new(State {
        #[cfg(feature="gtts")] gtts: gtts::State::new().await?,
        #[cfg(feature="premium")] premium: premium::State::new()?,
        redis: std::env::var("REDIS_URI").ok().map(|uri| {
            let key = std::env::var("CACHE_KEY").expect("CACHE_KEY not set!");
            RedisCache {
                client: deadpool_redis::Config::from_url(uri).create_pool(Some(deadpool_redis::Runtime::Tokio1)).unwrap(),
                key: fernet::Fernet::new(&key).unwrap()
            }
        }),
    });

    let app = axum::Router::new()
        .route("/tts", axum::routing::get({
            let shared_state = Arc::clone(&state);
            move |q| get_tts(Arc::clone(&shared_state), q)
        }))
        .route("/voices", axum::routing::get(get_voices))
        .route("/modes", axum::routing::get(|| async {
            axum::Json([
                #[cfg(feature="gtts")] "gTTS",
                #[cfg(feature="espeak")] "eSpeak",
                #[cfg(feature="premium")] "Premium",
            ])
        }));

    let bind_to = std::env::var("BIND_ADDR").ok().map_or_else(
        || Cow::Borrowed("0.0.0.0:3000"),
        Cow::Owned
    ).parse()?;

    tracing::info!("Binding to {bind_to} {} redis enabled!", if state.redis.is_some() {"with"} else {"without"});
    axum::Server::bind(&bind_to)
        .serve(app.into_make_service())
        .with_graceful_shutdown(async {drop(tokio::signal::ctrl_c().await)})
        .await?;

    Ok(())
}


#[derive(Debug)]
struct Error {
    inner: anyhow::Error
}

impl<E> From<E> for Error
where E: Into<anyhow::Error> {
    fn from(err: E) -> Self {
        Self {inner: err.into()}
    }
}

impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        tracing::error!("{self:?}");
        axum::response::Response::builder()
            .status(500)
            .body(axum::body::boxed(axum::body::Full::from(format!("{:?}", self.inner))))
            .unwrap()
    }
}
