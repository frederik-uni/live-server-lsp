use std::time::Duration;

use dashboard::{check_server, start_server};
use lsp::lsp;
use reqwest::Client;
use tokio::time::sleep;

mod dashboard;
pub mod lsp;

#[rocket::main]
async fn main() {
    let port = 57391;
    if !check_server(
        &Client::new(),
        format!("http://127.0.1:{}", port).parse().unwrap(),
    )
    .await
    {
        tokio::spawn(start_server(port));
        sleep(Duration::from_secs(1)).await;
    }

    lsp(port, true, true).await;
}
