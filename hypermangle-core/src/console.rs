use std::{ffi::OsString, io::Write};

use clap::{crate_name, Parser};
use futures::AsyncReadExt;
use interprocess::local_socket::tokio::{LocalSocketListener, LocalSocketStream};
use log::error;
use serde::{Deserialize, Serialize};

pub use futures::{AsyncWrite, AsyncWriteExt};

#[derive(Serialize, Deserialize)]
enum BaseCommand {
    Id,
    Args(Vec<OsString>),
}

fn get_socket_name() -> String {
    format!("{}.sock", crate_name!())
}

#[tokio::main(flavor = "current_thread")]
pub async fn does_remote_exist() -> Option<u32> {
    let Ok(mut stream) = LocalSocketStream::connect(get_socket_name()).await else {
        return None;
    };
    send_msg(BaseCommand::Id, &mut stream)
        .await
        .expect("Remote Server should have responded with its Process ID");
    let mut msg = [0u8; 4];
    stream
        .read_exact(&mut msg)
        .await
        .expect("Remote Server should have responded with its Process ID");
    Some(u32::from_ne_bytes(msg))
}

async fn send_msg(msg: BaseCommand, stream: &mut LocalSocketStream) -> std::io::Result<()> {
    let mut msg = bincode::serialize(&msg).unwrap();

    let mut tmp = msg.len().to_ne_bytes().to_vec();
    tmp.append(&mut msg);
    msg = tmp;

    stream.write_all(&msg).await
}

#[tokio::main(flavor = "current_thread")]
pub async fn send_args_to_remote() {
    let mut stream = LocalSocketStream::connect(get_socket_name())
        .await
        .expect("Connection to remote service should have succeeded");

    send_msg(
        BaseCommand::Args(std::env::args_os().collect()),
        &mut stream,
    )
    .await
    .expect("Remote Server should have accepted the given arguments");

    let mut stdout = std::io::stdout();
    loop {
        let mut buf = [0u8; 1024];
        let Ok(n) = stream.read(&mut buf).await else {
            break;
        };
        if stdout.write_all(buf.split_at(n).0).is_err() {
            break;
        };
    }
}

pub trait ExecutableArgs: Parser {
    async fn execute<W: AsyncWrite + Unpin>(self, writer: W);
}

pub async fn listen_for_commands<P: ExecutableArgs>() {
    let listener = LocalSocketListener::bind(get_socket_name())
        .expect("Command listener should have started successfully");
    loop {
        let mut stream = match listener.accept().await {
            Ok(x) => x,
            Err(e) => {
                error!("Faced the following error while listening for commands: {e}");
                continue;
            }
        };

        macro_rules! unwrap {
            ($result: expr) => {
                match $result {
                    Ok(x) => x,
                    Err(e) => {
                        error!("Faced the following error while listening for commands: {e}");
                        let _ = stream.write_all(e.to_string().as_bytes()).await;
                        continue;
                    }
                }
            };
        }

        let mut msg_size = [0u8; (usize::BITS / 8) as usize];
        unwrap!(stream.read_exact(&mut msg_size).await);
        let msg_size = usize::from_ne_bytes(msg_size);
        let mut msg = vec![0u8; msg_size];
        unwrap!(stream.read_exact(&mut msg).await);

        let msg: BaseCommand = unwrap!(bincode::deserialize(&msg));

        match msg {
            BaseCommand::Id => {
                unwrap!(stream.write_all(&std::process::id().to_ne_bytes()).await);
            }
            BaseCommand::Args(args) => {
                let args = match P::try_parse_from(args) {
                    Ok(x) => x,
                    Err(e) => {
                        unwrap!(stream.write_all(e.to_string().as_bytes()).await);
                        continue;
                    }
                };
                args.execute(stream).await;
            }
        }
    }
}
