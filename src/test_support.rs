//! Test doubles shared by the bridge's tests.

use std::sync::{Arc, Mutex};

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

/// A TCP relay between the bridge and the broker that a test can cut, as a network fault would:
/// the bridge sees its connection drop, and reconnects through the relay.
pub struct Relay {
    pub address: String,
    links: Arc<Mutex<Vec<JoinHandle<()>>>>,
    task: JoinHandle<()>,
}

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        self.cut();
    }
}

impl Relay {
    pub async fn start(upstream: String) -> Relay {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the relay");
        let address = listener.local_addr().expect("the relay's address").to_string();
        let links = Arc::new(Mutex::new(Vec::new()));
        let held = links.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let upstream = upstream.clone();
                let link = tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(&upstream).await {
                        let _ = server.set_nodelay(true);
                        let _ = client.set_nodelay(true);
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
                held.lock().unwrap().push(link);
            }
        });
        Relay { address, links, task }
    }

    /// Drop every open connection through the relay.
    pub fn cut(&self) {
        for link in self.links.lock().unwrap().drain(..) {
            link.abort();
        }
    }
}
