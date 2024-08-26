use clap::Parser;
use lsp::lsp;

pub mod lsp;

#[derive(Parser, Debug)]
#[command(name = "LiveServer")]
#[command(about = "configure the live server")]
struct Cli {
    /// Set the eager flag
    #[arg(short, long)]
    eager: bool,

    /// Set the public flag
    #[arg(long)]
    public: bool,

    /// Set the port number
    #[arg(short, long, value_name = "PORT")]
    port: Option<u16>,
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();
    let port = args.port.unwrap_or(57391);

    lsp(port, args.public, args.eager).await;
}
