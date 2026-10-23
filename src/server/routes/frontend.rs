use std::convert::Infallible;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};

use crate::embed;

/// 内嵌前端静态服务 + SPA fallback（/admin* → admin.html，其余 → index.html）
pub async fn static_handler(uri: Uri, _req: Request) -> Result<Response, Infallible> {
    let path = uri.path();
    if let Some((bytes, mime)) = embed::lookup(path) {
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .body(Body::from(bytes))
            .unwrap());
    }
    if !path.starts_with("/api/") {
        if let Some((bytes, mime)) = embed::fallback(path) {
            return Ok(Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, mime)
                .body(Body::from(bytes))
                .unwrap());
        }
    }
    Ok((StatusCode::NOT_FOUND, Html("<h1>404 Not Found</h1>".to_string())).into_response())
}
