use flame_cluster::{AppState, connection::load_connections};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let configs = load_connections()?;
    let kube_client = if configs.iter().any(|(_, config)| {
        config.namespace.is_some()
            || config.join_secret.is_some()
            || config.client_config_secret.is_some()
    }) {
        Some(kube::Client::try_default().await?)
    } else {
        None
    };
    let connections = configs
        .into_iter()
        .map(|(id, config)| {
            Ok((
                id,
                AppState::connect_with_kubernetes(config, kube_client.clone())?,
            ))
        })
        .collect::<Result<Vec<_>, Box<dyn std::error::Error + Send + Sync>>>()?;
    let app = flame_cluster::fleet_router(connections)?;
    let address = std::env::var("CONSOLE_BIND").unwrap_or_else(|_| "127.0.0.1:3000".into());
    let listener = tokio::net::TcpListener::bind(&address).await?;
    println!("Flame Cluster API: http://{}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
