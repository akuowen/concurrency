use std::fs::File;
use std::{env, fmt, net::SocketAddr, sync::Arc};

use anyhow::{bail, Result};
use dashmap::DashMap;
use futures::{
    stream::{SplitStream, StreamExt},
    SinkExt,
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_util::codec::{Framed, LinesCodec};
use tracing::{info, warn};

#[derive(Debug, Clone)]
struct State {
    server: ServerConfig,
    peers: DashMap<SocketAddr, mpsc::Sender<Arc<Message>>>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl State {
    async fn new_tcp_listener(&self) -> Result<TcpListener> {
        let addr = format!("{}:{}", self.server.host, self.server.port);
        Ok(TcpListener::bind(addr).await?)
    }

    async fn try_load() -> Result<Self> {
        let config = match (
            File::open("config.yaml"),
            File::open("/etc/config.yaml"),
            env::var("APP_CONFIG_PATH"),
        ) {
            (Ok(reader), _, _) => serde_yaml::from_reader(reader),
            (_, Ok(reader), _) => serde_yaml::from_reader(reader),
            (_, _, Ok(path)) => serde_yaml::from_reader(File::open(path)?),
            _ => bail!(anyhow::anyhow!("Config file not found")),
        };

        Ok(State {
            server: config?,
            peers: DashMap::new(),
        })
    }

    async fn broadcast(&self, addr: SocketAddr, message: Arc<Message>) {
        for peer in self.peers.iter() {
            if peer.key() == &addr {
                continue;
            }

            if peer.value().send(message.clone()).await.is_err() {
                info!("Failed to send message to peer: {:?}", peer.key());
                // remove the peer
                self.peers.remove(peer.key());
            }
        }
    }

    async fn add_peer(
        &self,
        addr: SocketAddr,
        username: String,
        stream: Framed<TcpStream, LinesCodec>,
    ) -> Peer {
        let (tx, mut rx) = mpsc::channel(16);

        self.peers.insert(addr, tx);

        let (mut sender, receiver) = stream.split();

        tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if sender.send(message.to_string()).await.is_err() {
                    info!("Failed to send message to peer: {:?}", addr);
                    break;
                }
            }
        });

        Peer {
            username,
            stream: receiver,
        }
    }
}

#[derive(Debug, Clone)]
struct Message {
    sender: String,
    content: String,
}

#[derive(Debug)]
struct Peer {
    username: String,
    stream: SplitStream<Framed<TcpStream, LinesCodec>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let state = State::try_load().await?;
    let listener = state.new_tcp_listener().await?;

    loop {
        let (socket, addr) = listener.accept().await?;
        info!("Accepted connection from: {}", addr);
        let clone_state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(clone_state, addr, socket).await {
                warn!("failed to handle connection: {:?}", e);
            }
        });
    }

    #[allow(unreachable_code)]
    Ok(())
}

async fn handle_connection(state: State, addr: SocketAddr, socket: TcpStream) -> Result<()> {
    let mut framed = Framed::new(socket, LinesCodec::new());
    framed.send("Enter your username:").await?;
    let username = match framed.next().await {
        Some(Ok(username)) => username,
        _ => {
            warn!("Failed to get username from peer: {:?}", addr);
            return Ok(());
        }
    };

    framed.send(format!("Welcome, {}!", username)).await?;
    state
        .broadcast(
            addr,
            Arc::new(Message {
                sender: "Server".to_string(),
                content: format!("{} has joined the chat.", username),
            }),
        )
        .await;

    let mut peer = state.add_peer(addr, username, framed).await;
    while let Some(line) = peer.stream.next().await {
        let line = line.unwrap();
        state
            .broadcast(
                addr,
                Arc::new(Message {
                    sender: peer.username.clone(),
                    content: line,
                }),
            )
            .await;
    }

    state
        .broadcast(
            addr,
            Arc::new(Message {
                sender: "Server".to_string(),
                content: format!("{} has left the chat.", peer.username),
            }),
        )
        .await;

    Ok(())
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}: {}", self.sender, self.content)
    }
}
