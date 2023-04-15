use std::net::SocketAddr;

use anyhow::Result;
use atrium::server::Server;

pub const CONFIG_FILE: &str = "atrium.yaml";

#[tokio::main]
async fn main() -> Result<()> {
    let server = Server::build(CONFIG_FILE).await.unwrap();
    let addr = format!("[::]:8080") // On linux bind to ipv6 binds to ipv4 as well
        .parse::<std::net::SocketAddr>()
        .unwrap();
    let app = server
        .router
        .into_make_service_with_connect_info::<SocketAddr>();

    axum_server::bind(addr).serve(app).await.unwrap();
    Ok(())
}
