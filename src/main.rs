use axum::body::Body;
use axum::extract::Path;
use axum::http::header;
use axum::response::IntoResponse;
use axum::{routing::get, Router};
use std::path::PathBuf;
use std::sync::LazyLock;
use tokio::fs;
use tokio_util::io::ReaderStream;

static STATIC_PATH: LazyLock<PathBuf> = LazyLock::new(|| {
    let path = std::env::var("STATIC_PATH").unwrap_or("/static".to_string());
    PathBuf::from(path)
});

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/{*file}", get(serve));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    tracing::info!("Server running on http://127.0.0.1:3000");

    axum::serve(listener, app).await.unwrap();
}

// 处理函数
async fn serve(Path(file): Path<String>) -> impl IntoResponse {
    let path = STATIC_PATH.join(file);
    println!("path:{path:?}");
    let meta = fs::metadata(&path).await.unwrap();
    let file = fs::OpenOptions::new().read(true).open(path).await.unwrap();
    let stream = ReaderStream::new(file);

    let body = Body::from_stream(stream);

    let headers = [(header::CONTENT_TYPE, "text/html")];

    println!("file:{meta:?}");
    (headers, body)
}

async fn health_check() -> &'static str {
    "OK"
}
