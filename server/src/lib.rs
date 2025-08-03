use axum::{
  http::{
    header,
    Request,
  },
  body::Body,
  response::Response,
  routing::{get, post},
  Router,
};
use entities::missing_track::MissingTrack;
use repositories::lyrics_repository::get_last_10_mins_lyrics_count;
use tracing_subscriber::EnvFilter;
use std::{path::PathBuf, time::Duration};
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use routes::{
  get_lyrics_by_metadata,
  get_lyrics_by_track_id,
  search_lyrics,
  request_challenge,
  publish_lyrics,
  flag_lyrics,
};
use std::sync::Arc;
use db::init_db;
use tower_http::{
  cors::{Any, CorsLayer}, trace::{self, TraceLayer}
};
use tracing::Span;
use moka::future::Cache;
use tokio::signal;
use queue::start_queue;
use std::sync::atomic::{AtomicUsize, Ordering};
use crossbeam_queue::ArrayQueue;

pub mod errors;
pub mod routes;
pub mod entities;
pub mod repositories;
pub mod utils;
pub mod db;
pub mod queue;
pub mod providers;

pub struct AppState {
  pool: Pool<SqliteConnectionManager>,
  challenge_cache: Cache<String, String>,
  get_cache: Cache<String, String>,
  search_cache: Cache<String, String>,
  queue: ArrayQueue<MissingTrack>,
  request_counter: AtomicUsize,
  recent_lyrics_count: AtomicUsize,
}

pub async fn serve(port: u16, database: &PathBuf, workers_count: u8) {
  tracing_subscriber::fmt()
    .compact()
    .with_env_filter(EnvFilter::from_env("LRCLIB_LOG"))
    .init();

  let pool = init_db(database).expect("Cannot initialize connection to SQLite database!");

  let state = Arc::new(
    AppState {
      pool,
      challenge_cache: Cache::<String, String>::builder()
        .time_to_live(Duration::from_secs(60 * 5))
        .max_capacity(100000)
        .build(),
      get_cache: Cache::<String, String>::builder()
        .time_to_live(Duration::from_secs(60 * 60 * 24 * 7))
        .max_capacity(5000000)
        .build(),
      search_cache: Cache::<String, String>::builder()
        .time_to_live(Duration::from_secs(60 * 60 * 24))
        .time_to_idle(Duration::from_secs(60 * 60 * 4))
        .max_capacity(400000)
        .build(),
      queue: ArrayQueue::new(600000),
      request_counter: AtomicUsize::new(0),
      recent_lyrics_count: AtomicUsize::new(0),
    }
  );

  let state_for_logging = state.clone();
  let state_for_metrics = state.clone();
  let state_for_recent_lyrics_count = state.clone();
  let state_for_queue = state.clone();

  let api_routes = Router::new()
    .route("/get", get(get_lyrics_by_metadata::route))
    .route("/get/:track_id", get(get_lyrics_by_track_id::route))
    .route("/search", get(search_lyrics::route))
    .route("/request-challenge", post(request_challenge::route))
    .route("/publish", post(publish_lyrics::route))
    .route("/flag", post(flag_lyrics::route));

  // Metrics
  tokio::spawn(async move {
    tokio::time::sleep(Duration::from_secs(60)).await;
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
      interval.tick().await;
      let count = state_for_metrics.request_counter.swap(0, Ordering::Relaxed);
      tracing::info!(message = "requests in the last minute", requests_count = count);
    }
  });

  // Recent lyrics count
  tokio::spawn(async move {
    tokio::time::sleep(Duration::from_secs(60)).await;
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
      interval.tick().await;
      let mut conn = state_for_recent_lyrics_count.pool.get().unwrap();
      let count = get_last_10_mins_lyrics_count(&mut conn).unwrap();
      state_for_recent_lyrics_count.recent_lyrics_count.store(count as usize, Ordering::Relaxed);
    }
  });

  let app = Router::new()
    .nest("/api", api_routes)
    .with_state(state)
    .layer(
      TraceLayer::new_for_http()
        .make_span_with(|request: &Request<Body>| {
          let headers = request.headers();
          let user_agent = headers
            .get("Lrclib-Client")
            .and_then(|value| value.to_str().ok())
            .or_else(|| headers.get("X-User-Agent").and_then(|value| value.to_str().ok()))
            .or_else(|| headers.get(header::USER_AGENT).and_then(|value| value.to_str().ok()))
            .unwrap_or("");
          let method = request.method().to_string();
          let uri = request.uri().to_string();

          tracing::debug_span!("request", method, uri, user_agent)
        })
        .on_response(|response: &Response, latency: Duration, _span: &Span| {
          let status_code = response.status().as_u16();
          let latency = latency.as_millis();

          if latency > 500 {
            tracing::info!(
              message = "finished processing request",
              slow = true,
              latency = latency,
              status_code = status_code,
            )
          } else {
            tracing::debug!(
              message = "finished processing request",
              latency = latency,
              status_code = status_code,
            )
          }
        })
        .on_failure(trace::DefaultOnFailure::new().level(tracing::Level::ERROR))
        .on_request(move |_request: &Request<Body>, _span: &Span| {
          state_for_logging.request_counter.fetch_add(1, Ordering::Relaxed);
        })
    )
    .layer(
      CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers([
          header::CONTENT_TYPE,
          "X-User-Agent".parse().unwrap(),
          "Lrclib-Client".parse().unwrap()
        ])
    );

  tokio::spawn(async move {
    start_queue(workers_count, state_for_queue).await;
  });

  let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{}", port)).await.unwrap();
  println!("LRCLIB server is listening on {}!", listener.local_addr().unwrap());
  axum::serve(listener, app)
    .with_graceful_shutdown(shutdown_signal())
    .await
    .unwrap();
}

async fn shutdown_signal() {
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>(); // para hindi mag-error sa Windows

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    tokio::select! {
        _ = ctrl_c => {
            println!("CTRL+C pressed, exiting...");
        },
        _ = terminate => {
            println!("Terminate signal received, exiting...");
        },
    }
}

