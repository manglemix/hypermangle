#![feature(async_fn_in_trait)]

use axum::Router;
use clap::Parser;
use hypermangle_core::{
    auto_main,
    console::{AsyncWrite, AsyncWriteExt, ExecutableArgs},
};

#[derive(Parser)]
struct Args {}

impl ExecutableArgs for Args {
    async fn execute<W: AsyncWrite + Unpin>(self, mut writer: W) -> bool {
        let _ = writer.write_all("Killing...".as_bytes()).await;
        true
    }
}

fn main() {
    auto_main::<Args>(|| Router::new());
}
