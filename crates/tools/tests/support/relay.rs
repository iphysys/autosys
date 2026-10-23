//! A TCP relay between the gateway and a real MCP server. It counts the HTTP
//! requests that pass, so a test can prove a call never reached the server,
//! and can hold the server's replies back, so a test can make a real call
//! time out.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

const REQUEST_LINE: &[u8] = b"POST /mcp";

pub struct Relay {
    addr: SocketAddr,
    posts: Arc<AtomicUsize>,
    delay_ms: Arc<AtomicU64>,
    accepting: JoinHandle<()>,
}

impl Relay {
    /// Listens on a free loopback port and relays every connection to
    /// `target` (`host:port`).
    pub async fn start(target: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind relay");
        let addr = listener.local_addr().expect("relay address");
        let posts = Arc::new(AtomicUsize::new(0));
        let delay_ms = Arc::new(AtomicU64::new(0));
        let accepting = {
            let posts = posts.clone();
            let delay_ms = delay_ms.clone();
            tokio::spawn(async move {
                while let Ok((client, _)) = listener.accept().await {
                    let target = target.clone();
                    let posts = posts.clone();
                    let delay_ms = delay_ms.clone();
                    tokio::spawn(async move {
                        if let Ok(server) = TcpStream::connect(&target).await {
                            relay(client, server, posts, delay_ms).await;
                        }
                    });
                }
            })
        };
        Self {
            addr,
            posts,
            delay_ms,
            accepting,
        }
    }

    /// The MCP endpoint through the relay.
    pub fn url(&self) -> String {
        format!("http://{}/mcp", self.addr)
    }

    /// HTTP requests to `/mcp` relayed so far.
    pub fn posts(&self) -> usize {
        self.posts.load(Ordering::SeqCst)
    }

    /// Holds every chunk of the server's replies back by `delay`.
    pub fn delay_replies(&self, delay: Duration) {
        let ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
        self.delay_ms.store(ms, Ordering::SeqCst);
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.accepting.abort();
    }
}

async fn relay(
    client: TcpStream,
    server: TcpStream,
    posts: Arc<AtomicUsize>,
    delay_ms: Arc<AtomicU64>,
) {
    let (mut client_read, mut client_write) = client.into_split();
    let (mut server_read, mut server_write) = server.into_split();
    let upstream = async move {
        let mut buf = vec![0u8; 16 * 1024];
        // The end of the previous chunk, so a request line split across two
        // reads is still counted once.
        let mut carry: Vec<u8> = Vec::new();
        loop {
            let n = match client_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let mut window = std::mem::take(&mut carry);
            window.extend_from_slice(&buf[..n]);
            let found = window
                .windows(REQUEST_LINE.len())
                .filter(|w| *w == REQUEST_LINE)
                .count();
            posts.fetch_add(found, Ordering::SeqCst);
            let keep = REQUEST_LINE.len() - 1;
            carry = window[window.len().saturating_sub(keep)..].to_vec();
            if server_write.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = server_write.shutdown().await;
    };
    let downstream = async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = match server_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let delay = delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if client_write.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = client_write.shutdown().await;
    };
    tokio::join!(upstream, downstream);
}
