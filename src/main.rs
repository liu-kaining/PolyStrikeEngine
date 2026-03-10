mod config;
mod engine;
mod execution;
mod models;
mod oracle;
mod risk;
mod strategy;

fn main() -> anyhow::Result<()> {
    // Required by rustls 0.23+ when used via tokio-tungstenite (wss://).
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install rustls default crypto provider");

    let _ = dotenvy::dotenv();
    println!("[PolyStrike Engine] Starting...");

    let rt = tokio::runtime::Runtime::new().expect("create tokio runtime");
    rt.block_on(engine::run())
}

