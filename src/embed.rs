use std::collections::HashMap;
use std::sync::OnceLock;

use macro_log::read_dir;

/// 前端构建产物（web/dist），编译期嵌入。目录不存在时为空数组，开发期友好。
static ASSETS: &[(&str, &[u8])] = &read_dir!("web/dist");

static ASSET_MAP: OnceLock<HashMap<String, &'static [u8]>> = OnceLock::new();

fn asset_map() -> &'static HashMap<String, &'static [u8]> {
    ASSET_MAP.get_or_init(|| {
        ASSETS
            .iter()
            .map(|(path, bytes)| (path.replace('\\', "/"), *bytes))
            .collect()
    })
}

pub fn lookup(path: &str) -> Option<(&'static [u8], &'static str)> {
    let map = asset_map();
    if map.is_empty() {
        return None;
    }
    let clean = path.trim_start_matches('/');
    if clean.is_empty() {
        return map.get("index.html").map(|b| (*b, "text/html; charset=utf-8"));
    }
    if let Some(bytes) = map.get(clean) {
        return Some((*bytes, mime_of(clean)));
    }
    None
}

/// SPA fallback：/admin* 落 admin.html，其余落 index.html
pub fn fallback(path: &str) -> Option<(&'static [u8], &'static str)> {
    let map = asset_map();
    if map.is_empty() {
        return None;
    }
    let file = if path.starts_with("/admin") {
        "admin.html"
    } else {
        "index.html"
    };
    map.get(file)
        .map(|b| (*b, "text/html; charset=utf-8"))
}

// ---- MIME 推断（避免引入 mime_guess 整包）----

fn mime_of(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();
    match ext.as_str() {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "wasm" => "application/wasm",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}
