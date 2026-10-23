use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::StatusCode;
use std::sync::Arc;

use super::state::AppState;

/// 管理员认证：Authorization: Bearer <session_token>
pub struct AdminAuth;

impl FromRequestParts<Arc<AppState>> for AdminAuth {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or((StatusCode::UNAUTHORIZED, "缺少管理员会话凭证"))?;
        if !state.session_valid(token) {
            return Err((StatusCode::UNAUTHORIZED, "会话无效或已过期"));
        }
        Ok(AdminAuth)
    }
}

/// 客户端凭证：X-Client-Token 头，服务端校验后返回已批准的记录
pub struct ClientAuth(pub crate::server::store::ClientRecord);

impl FromRequestParts<Arc<AppState>> for ClientAuth {
    type Rejection = (StatusCode, &'static str);

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get("x-client-token")
            .and_then(|v| v.to_str().ok())
            .filter(|t| !t.is_empty())
            .ok_or((StatusCode::UNAUTHORIZED, "缺少客户端凭证"))?;
        let record = state
            .store
            .find_by_token(token)
            .ok_or((StatusCode::UNAUTHORIZED, "客户端凭证无效或已被吊销"))?;
        Ok(ClientAuth(record))
    }
}
