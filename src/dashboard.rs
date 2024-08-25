use std::fmt::Display;
use std::net::TcpListener;
use std::{sync::Arc, time::Duration};

use reqwest::Client;
use rocket::futures::SinkExt;
use rocket::response::content::RawHtml;
use rocket::serde::json::Json;
use rocket::serde::{Deserialize, Serialize};
use rocket::tokio::sync::RwLock;
use rocket::tokio::time::interval;
use rocket::{
    fairing::{Fairing, Info, Kind},
    http::Method,
    post, routes, Data, Request, State,
};
use rocket::{get, tokio, Config, Rocket};
use rocket_include_static_resources::{static_resources_initializer, static_response_handler};
use tokio::sync::broadcast::{channel, Receiver, Sender};
use url::Url;

#[derive(Serialize, Deserialize)]
pub struct Server {
    pub name: String,
    pub server: Option<String>,
    pub port: u16,
}

#[derive(Default)]
struct ServerRegistry {
    servers: Vec<(String, Url)>,
}

static_response_handler! {
    "/favicon.ico" => favicon => "favicon",
}

#[get("/")]
async fn index(registry: &State<Arc<RwLock<ServerRegistry>>>) -> RawHtml<String> {
    let items = registry.read().await;
    let content = serde_json::to_string(&items.servers).unwrap();
    RawHtml(include_str!("../dashboard.html").replace("{ insert_here }", &content))
}

#[post("/ping")]
fn ping() -> &'static str {
    "pong"
}

#[post("/register", format = "json", data = "<server>")]
async fn register(
    server: Json<Server>,
    registry: &State<Arc<RwLock<ServerRegistry>>>,
    sender: &State<Arc<Sender<Sent>>>,
) -> &'static str {
    let mut url = Url::parse(
        &server
            .server
            .clone()
            .unwrap_or("http://127.0.0.1".to_string()),
    )
    .unwrap();
    let _ = url.set_port(Some(server.port));
    if check_server(&Client::new(), url.clone()).await {
        let mut reg = registry.write().await;
        reg.servers.push((server.name.clone(), url.clone()));
        let _ = sender.send(Sent {
            added: true,
            url: url.to_string(),
            name: server.name.clone(),
        });
    }

    ""
}

#[post("/ports")]
async fn ports(registry: &State<Arc<RwLock<ServerRegistry>>>) -> Json<Vec<(String, String)>> {
    let reg = registry.read().await;
    Json(
        reg.servers
            .iter()
            .map(|v| (v.0.to_string(), v.1.to_string()))
            .collect(),
    )
}

struct LocalhostGuard;
#[rocket::async_trait]
impl Fairing for LocalhostGuard {
    fn info(&self) -> Info {
        Info {
            name: "localhost only",
            kind: Kind::Request,
        }
    }
    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data<'_>) {
        if !request
            .client_ip()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
        {
            request.set_method(Method::Options);
        }
    }
}
fn can_open_port(port: u16) -> bool {
    let address = format!("127.0.0.1:{}", port);
    TcpListener::bind(address).is_ok()
}

pub async fn get_port(client: &Client, mut port: u16) -> u16 {
    let data: Vec<(String, String)> = client
        .post(format!("http://127.0.0.1:{}/ports", port))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    port += 1;
    while data
        .iter()
        .find(|(_, p)| p == &format!("http://127.0.0.1:{}", port))
        .is_some()
        || !can_open_port(port)
    {
        port += 1;
    }

    port
}

pub async fn report_port(client: &Client, port: u16, report: Server) {
    client
        .post(&format!("http://127.0.0.1:{}/register", port))
        .json(&report)
        .send()
        .await
        .unwrap();
}

pub async fn start_server(port: u16) {
    let registry = Arc::new(RwLock::new(ServerRegistry::default()));
    let r_c = registry.clone();
    let (sender, receiver): (Sender<Sent>, Receiver<Sent>) = channel(10);
    let sender = Arc::new(sender);
    let s_c = sender.clone();
    tokio::task::spawn(async move {
        let mut interval = interval(Duration::from_secs(60));
        let client = Client::new();

        loop {
            interval.tick().await;
            let servers = r_c.read().await.servers.clone();
            let mut items = vec![];
            for (name, url) in servers {
                if !check_server(&client, url.clone()).await {
                    items.push((name, url));
                }
            }
            let mut reg = r_c.write().await;
            reg.servers.retain(|v| !items.contains(&v));
            for (name, item) in items {
                let _ = s_c.send(Sent {
                    added: false,
                    url: item.to_string(),
                    name,
                });
            }
        }
    });
    let _ = Rocket::custom(Config::figment().merge(("port", port)))
        .manage(registry)
        .manage(sender)
        .attach(static_resources_initializer!(
            "favicon" => "./favicon.ico",
        ))
        .manage(Arc::new(receiver))
        .mount("/", routes![favicon])
        .mount("/", routes![index])
        .mount("/", routes![ping, register, ports])
        .mount("/", routes![web_socket])
        .attach(LocalhostGuard)
        .launch()
        .await;
}

pub async fn check_server(client: &Client, mut url: Url) -> bool {
    url.set_path("/ping");
    client.post(url).send().await.is_ok()
}

#[derive(Clone)]
struct Sent {
    added: bool,
    name: String,
    url: String,
}

impl Display for Sent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            r#"{}"added":{},"name":"{}","url":"{}"{}"#,
            '{', self.added, self.name, self.url, '}'
        )
    }
}

#[get("/ws")]
fn web_socket(ws: ws::WebSocket, receiver: &State<Arc<Receiver<Sent>>>) -> ws::Channel<'static> {
    use rocket::futures::stream::StreamExt as FuturesStreamExt;
    let mut receiver = receiver.resubscribe();

    ws.channel(move |mut stream| {
        Box::pin(async move {
            loop {
                tokio::select! {
                    Some(_) = stream.next() => {
                        // Ignoring the client messages
                    }

                    Ok(url) = receiver.recv() => {
                        if let Err(_) = stream.send(ws::Message::Text(url.to_string())).await {
                            break;
                        }
                    }

                    else => break, // Exit the loop if either stream ends
                }
            }
            Ok(())
        })
    })
}
