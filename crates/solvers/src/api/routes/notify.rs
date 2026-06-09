use axum::{Json, http::StatusCode};

pub async fn notify(Json(notification): Json<serde_json::Value>) -> StatusCode {
    tracing::info!(%notification, "driver notification");
    StatusCode::OK
}
