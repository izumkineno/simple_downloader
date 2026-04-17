use simple_downloader::reqwest::ClientBuilder;
use simple_downloader::{Downloader, MultiSourceConfig, SourceConfig};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("[resume_harness] {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let Some(mode) = args.first().map(String::as_str) else {
        return Err("usage: resume_harness <single|multi> ...".into());
    };

    match mode {
        "single" => run_single(&args[1..]).await,
        "multi" => run_multi(&args[1..]).await,
        other => Err(format!("unsupported mode: {other}").into()),
    }
}

async fn run_single(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if args.len() != 4 {
        return Err(
            "usage: resume_harness single <url> <output> <workers> <update_interval>".into(),
        );
    }

    let url = args[0].clone();
    let output = args[1].clone();
    let workers = args[2].parse::<u64>()?;
    let update_interval = args[3].parse::<f64>()?;

    let download = Downloader::new(url, output, workers, update_interval, ClientBuilder::new)
        .run(|_, _| async {});
    tokio::pin!(download);

    tokio::select! {
        result = &mut download => {
            result?;
            Ok(())
        }
        _ = interruption_signal() => {
            std::process::exit(130);
        }
    }
}

async fn run_multi(args: &[String]) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if args.len() < 4 {
        return Err(
            "usage: resume_harness multi <output> <workers> <update_interval> <source1> <source2>...".into(),
        );
    }

    let output = args[0].clone();
    let workers = args[1].parse::<u64>()?;
    let update_interval = args[2].parse::<f64>()?;
    let sources = args[3..]
        .iter()
        .enumerate()
        .map(|(index, url)| SourceConfig::new(url.clone()).with_id(format!("source-{index}")))
        .collect::<Vec<_>>();

    let config = MultiSourceConfig::new(output, workers, update_interval).with_sources(sources);
    let download = Downloader::new_multi(config, ClientBuilder::new).run(|_, _| async {});
    tokio::pin!(download);

    tokio::select! {
        result = &mut download => {
            result?;
            Ok(())
        }
        _ = interruption_signal() => {
            std::process::exit(130);
        }
    }
}

#[cfg(windows)]
async fn interruption_signal() {
    use tokio::signal::windows::{ctrl_break, ctrl_c};

    let mut ctrl_c = ctrl_c().expect("install ctrl_c handler");
    let mut ctrl_break = ctrl_break().expect("install ctrl_break handler");
    tokio::select! {
        _ = ctrl_c.recv() => {}
        _ = ctrl_break.recv() => {}
    }
}

#[cfg(not(windows))]
async fn interruption_signal() {
    let _ = tokio::signal::ctrl_c().await;
}
