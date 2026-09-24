use anyhow::{Context, Result, bail};
use moenotes_assets::{
    config::Config,
    service::{App, router},
    worker,
};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    #[cfg(unix)]
    if args.get(1).is_some_and(|s| s == "media-exec") {
        use std::os::unix::process::CommandExt;
        #[cfg(target_os = "linux")]
        {
            let parent = nix::unistd::getppid();
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL)?;
            anyhow::ensure!(
                parent.as_raw() != 1 && nix::unistd::getppid() == parent,
                "media parent exited"
            );
        }
        let error = std::process::Command::new(args.get(2).context("missing media executable")?)
            .args(&args[3..])
            .exec();
        return Err(error.into());
    }
    if args.get(1).is_some_and(|s| s == "worker") {
        #[cfg(target_os = "linux")]
        {
            let parent = nix::unistd::getppid();
            nix::sys::prctl::set_pdeathsig(nix::sys::signal::Signal::SIGKILL)?;
            anyhow::ensure!(
                parent.as_raw() != 1 && nix::unistd::getppid() == parent,
                "worker parent exited"
            );
        }
        return worker::worker_entry(std::path::Path::new(
            args.get(2).context("missing worker job")?,
        ));
    }
    if args.get(1).is_some_and(|s| s == "--version") {
        println!("moenotes-assets {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.get(1).is_some_and(|s| s == "upload-tree") {
        anyhow::ensure!(
            args.len() == 4,
            "usage: moenotes-assets upload-tree TREE S3_CONFIG.toml"
        );
        let config: moenotes_assets::upload::Config =
            toml::from_str(&std::fs::read_to_string(&args[3])?)?;
        return tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(async {
                let cancel = tokio_util::sync::CancellationToken::new();
                let signal = cancel.clone();
                tokio::spawn(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    signal.cancel();
                });
                moenotes_assets::upload::upload(std::path::Path::new(&args[2]), config, cancel)
                    .await
            });
    }
    if args.get(1).is_some_and(|s| s == "export-tree") {
        anyhow::ensure!(
            (4..=7).contains(&args.len()),
            "usage: moenotes-assets export-tree DATA_DIR DESTINATION [SNAPSHOT] [copy|hardlink] [PROFILE]"
        );
        let snapshot = args
            .get(4)
            .filter(|s| s.as_str() != "-")
            .map(String::as_str);
        let mode = args.get(5).map(String::as_str).unwrap_or("copy");
        anyhow::ensure!(matches!(mode, "copy" | "hardlink"), "invalid copy mode");
        return tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
            let index=moenotes_assets::tree::export_profile(std::path::Path::new(&args[2]),std::path::Path::new(&args[3]),snapshot,mode=="hardlink",args.get(6).map(String::as_str).unwrap_or(worker::PROFILE)).await?;
            println!("{}",serde_json::json!({"objects":index.objects.len(),"schema":index.schema,"naming":index.naming}));Ok(())
        });
    }
    if args.len() != 3 || args[1] != "serve" {
        bail!("usage: moenotes-assets serve CONFIG.toml | --version")
    }
    let config: Config = toml::from_str(&std::fs::read_to_string(&args[2])?)?;
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(async{
        let addr=config.listen;let app=App::open(config).await?;let listener=tokio::net::TcpListener::bind(addr).await?;
        tracing::info!(address=%listener.local_addr()?,"unauthenticated asset service listening");
        let stop=app.clone();axum::serve(listener,router(app)).with_graceful_shutdown(async move{
            #[cfg(unix)]{let mut term=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");tokio::select!{_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}}
            #[cfg(not(unix))]let _=tokio::signal::ctrl_c().await;
            stop.stop().await;
        }).await?;Ok(())
    })
}
