use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Clone, Serialize, Deserialize)]
struct Item {
    name: String,
    qty: u32,
}

type Db = Arc<Mutex<Vec<Item>>>;

async fn root() -> Json<serde_json::Value> {
    Json(serde_json::json!({"ok": true, "service": "rust-api"}))
}

async fn list(State(db): State<Db>) -> Json<Vec<Item>> {
    Json(db.lock().unwrap().clone())
}

async fn create(State(db): State<Db>, Json(item): Json<Item>) -> (StatusCode, Json<Item>) {
    db.lock().unwrap().push(item.clone());
    (StatusCode::CREATED, Json(item))
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let db: Db = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/", get(root))
        .route("/items", get(list).post(create))
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(db);
    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await.unwrap();
    println!("listening on {port}");
    axum::serve(listener, app).await.unwrap();
}
