fn main() {
    // 前端构建产物变更时触发重编，保证 read_dir! 嵌入最新 dist
    println!("cargo:rerun-if-changed=web/dist");
}
