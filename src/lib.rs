#![cfg_attr(not(test), warn(clippy::unwrap_used))]
#![doc = include_str!("../README.crates.io.md")]

mod app;
mod archive;
mod auth;
mod blob;
mod builder;
mod cache;
mod cargo_integration;
mod cli;
mod digest;
mod image;
mod output_target;
mod progress;
mod registry;
mod tar_io;

use anyhow::{Context, Result, bail};
use progress::Progress;
use std::future::Future;

#[doc(hidden)]
pub async fn run() -> Result<()> {
    run_until_interrupted(app::run()).await
}

#[doc(hidden)]
pub async fn run_cargo() -> Result<()> {
    run_until_interrupted(cargo_integration::run()).await
}

async fn run_until_interrupted<F>(operation: F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    tokio::select! {
        result = operation => result,
        signal = tokio::signal::ctrl_c() => {
            signal.context("installing Ctrl-C handler")?;
            bail!("interrupted")
        }
    }
}

fn init_tracing(progress: &Progress) {
    let filter = tracing_subscriber::EnvFilter::try_from_env("RIB_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(progress.tracing_writer())
        .with_ansi(progress.tracing_ansi())
        .try_init();
}
