use lsp::lsp;
use serde::{Deserialize, Serialize};

pub mod lsp;

#[derive(Deserialize, Serialize, Default)]
pub struct Config {
    /// Set if update on save or keypress [Default: false]
    lazy: Option<bool>,
    /// 0.0.0.0 or 127.0.0.1 [Default: false]
    public: Option<bool>,
    /// Set the port number
    start_port: Option<u16>,
}

#[tokio::main]
async fn main() {
    lsp().await;
}
