mod ake;
mod cache;
mod crypto;
mod device;
mod epg;
mod konomi;
mod proxy;
mod receiver;
mod recording;
mod recording_api;
mod reservations;
mod server;
mod srm;
mod ts_file;
use anyhow::{Result, ensure};
use clap::{Parser, Subcommand};
use std::{net::Ipv4Addr, path::PathBuf};
use tokio::io::AsyncWriteExt;
#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Version,
    Serve {
        #[arg(long)]
        config: PathBuf,
    },
    Discover {
        #[arg(long)]
        source: Ipv4Addr,
    },
    Info {
        #[arg(long)]
        config: PathBuf,
    },
    List {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        recorded: bool,
    },
    Capture {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        service: Option<u64>,
        #[arg(long)]
        recorded: Option<String>,
        #[arg(long)]
        output: PathBuf,
        #[arg(long, default_value = "15")]
        seconds: u64,
        #[arg(long, default_value = "0")]
        position: f64,
    },
}
fn config(path: &std::path::Path) -> Result<device::Config> {
    ensure!(
        std::fs::metadata(path)?.len() <= 16384,
        "Configuration too large"
    );
    let mut c: device::Config =
        serde_json::from_str(std::fs::read_to_string(path)?.trim_start_matches('\u{feff}'))?;
    let parent = std::fs::canonicalize(path)?
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Configuration parent unavailable"))?
        .to_owned();
    if c.credentials.is_relative() {
        c.credentials = parent.join(&c.credentials);
    }
    if c.state_dir.is_relative() {
        c.state_dir = parent.join(&c.state_dir);
    }
    if c.recording["enabled"] == true {
        for field in ["directory", "ffmpeg", "ffprobe"] {
            if let Some(value) = c.recording[field].as_str() {
                let p = PathBuf::from(value);
                if p.is_relative() {
                    c.recording[field] = serde_json::json!(
                        parent
                            .join(p)
                            .to_str()
                            .ok_or_else(|| anyhow::anyhow!("Invalid media path"))?
                    );
                }
            }
        }
    }
    device::lan_url(&c.location, None)?;
    ensure!(
        c.source.is_private() && c.listen.is_loopback() && (1..=2).contains(&c.max_streams),
        "Invalid network/slot configuration"
    );
    ensure!(
        ["original", "mobile1", "mobile2"].contains(&c.quality.as_str())
            && (c.max_streams == 1 || c.quality == "original"),
        "Invalid quality/slot combination"
    );
    Ok(c)
}
#[tokio::main]
async fn main() {
    if let Err(_) = run().await {
        eprintln!("Operation failed; protocol details and credential values are suppressed");
        std::process::exit(1)
    }
}
async fn run() -> Result<()> {
    match Args::parse().command {
        Command::Version => println!(
            "{}",
            serde_json::json!({"bridge":env!("CARGO_PKG_VERSION"),"openssl":openssl::version::version()})
        ),
        Command::Serve { config: path } => server::serve(config(&path)?).await?,
        Command::Discover { source } => println!(
            "{}",
            serde_json::to_string_pretty(&device::discover(source).await?)?
        ),
        Command::Info { config: path } => {
            let mut d = device::Device::new(config(&path)?)?;
            d.identify().await?;
            println!(
                "{}",
                serde_json::json!({"model":"XA401","identity_verified":true})
            );
        }
        Command::List {
            config: path,
            recorded,
        } => {
            let mut d = device::Device::new(config(&path)?)?;
            d.refresh().await?;
            let items = if recorded {
                d.recordings().await?
            } else {
                d.items
            };
            println!("{}", serde_json::to_string_pretty(&items)?);
        }
        Command::Capture {
            config: path,
            service,
            recorded,
            output,
            seconds,
            position,
        } => {
            ensure!(
                (1..=86400).contains(&seconds) && service.is_some() != recorded.is_some(),
                "Select one source and bounded capture time"
            );
            let c = config(&path)?;
            let credentials = crypto::Credentials::load(&c.credentials)?;
            let srm = c.state_dir.join("dtcp/srm.bin");
            let source = c.source;
            let mut d = device::Device::new(c)?;
            d.refresh().await?;
            let id = if let Some(token) = recorded {
                d.recordings()
                    .await?
                    .into_iter()
                    .find(|i| i.token == token)
                    .map(|i| i.object_id)
            } else {
                d.items
                    .iter()
                    .find(|i| i.service.as_ref().and_then(|v| v["id"].as_u64()) == service)
                    .map(|i| i.object_id.clone())
            }
            .ok_or_else(|| anyhow::anyhow!("Unknown source"))?;
            let resource = d.resource(&id).await?;
            let host = d.host;
            // Never overwrite any existing file, including a previous recording.
            let mut file = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)
                .await?;
            let (tx, mut rx) = tokio::sync::mpsc::channel(8);
            let task = tokio::spawn(async move {
                receiver::receive(source, host, &resource, &credentials, &srm, position, tx).await
            });
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
            let mut bytes = 0u64;
            while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, rx.recv()).await {
                file.write_all(&chunk).await?;
                bytes += chunk.len() as u64;
            }
            file.sync_all().await?;
            task.abort();
            let _ = task.await;
            ensure!(bytes >= 188 * 100, "No validated TS received");
            println!(
                "{}",
                serde_json::json!({"ts_bytes":bytes,"packet_length":188,"reencoded":false})
            );
        }
    }
    Ok(())
}
