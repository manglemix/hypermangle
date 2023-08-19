#![feature(async_fn_in_trait)]

use axum::Router;
use clap::Parser;
use hypermangle_core::{
    auto_main,
    console::{ExecutableArgs, RemoteClient},
};

#[derive(Parser)]
struct Args {}

impl ExecutableArgs for Args {
    async fn execute(self, mut writer: RemoteClient) -> bool {
        let _ = writer.send("Killing...".into()).await;
        true
    }
}

fn main() {
    auto_main::<Args>(|| Router::new());
}
