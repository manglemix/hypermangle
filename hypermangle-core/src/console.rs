use std::{ffi::OsString, mem::take};

use clap::{crate_name, Parser};
use futures::AsyncReadExt;
use interprocess::local_socket::tokio::{LocalSocketListener, LocalSocketStream};
use log::error;
use serde::{Deserialize, Serialize};

use futures::AsyncWriteExt;
use tokio::sync::mpsc;

pub struct RemoteClient {
    stream: Option<LocalSocketStream>,
}

impl RemoteClient {
    pub async fn send(&mut self, msg: String) {
        if let Err(e) = send_msg(BaseCommand::Packet(msg), self.stream.as_mut().unwrap()).await {
            error!("Faced the following error while responding to remote client: {e}");
        }
    }
}

impl Drop for RemoteClient {
    fn drop(&mut self) {
        let mut stream = take(&mut self.stream).unwrap();
        tokio::spawn(async move {
            if let Err(e) = send_msg(BaseCommand::CloseSocket, &mut stream).await {
                error!("Faced the following error while ending connection to remote client: {e}");
            }
        });
    }
}

#[derive(Serialize, Deserialize)]
enum BaseCommand {
    IdRequest,
    IdResponse(u32),
    Args(Vec<OsString>),
    Packet(String),
    CloseSocket,
}

fn get_socket_name() -> String {
    format!("/run/{}.sock", crate_name!())
}

#[tokio::main(flavor = "current_thread")]
pub async fn does_remote_exist() -> Option<u32> {
    let Ok(mut stream) = LocalSocketStream::connect(get_socket_name()).await else {
        return None;
    };
    send_msg(BaseCommand::IdRequest, &mut stream).await.ok()?;
    let Ok(BaseCommand::IdResponse(id)) = recv_msg(&mut stream).await else {
        panic!("Remote service should have responded with is Process ID")
    };
    Some(id)
}

async fn send_msg(msg: BaseCommand, stream: &mut LocalSocketStream) -> std::io::Result<()> {
    let mut msg = bincode::serialize(&msg).unwrap();

    let mut tmp = msg.len().to_ne_bytes().to_vec();
    tmp.append(&mut msg);
    msg = tmp;

    stream.write_all(&msg).await
}

async fn recv_msg(
    stream: &mut LocalSocketStream,
) -> Result<BaseCommand, Box<dyn std::error::Error>> {
    let mut msg_size = [0u8; (usize::BITS / 8) as usize];
    stream.read_exact(&mut msg_size).await.map_err(Box::new)?;
    let msg_size = usize::from_ne_bytes(msg_size);
    let mut msg = vec![0u8; msg_size];
    stream.read_exact(&mut msg).await.map_err(Box::new)?;

    bincode::deserialize(&msg).map_err(Into::into)
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
    .expect("Remote service should have accepted the given arguments");

    loop {
        let msg = recv_msg(&mut stream)
            .await
            .expect("Remote service should have sent a valid message");

        match msg {
            BaseCommand::Packet(msg) => print!("{msg}"),
            BaseCommand::CloseSocket => break,
            _ => {}
        }
    }
}

pub trait ExecutableArgs: Parser + Send + 'static {
    fn execute(self, writer: RemoteClient) -> impl std::future::Future<Output=bool> + Send;
}

pub fn listen_for_commands<P: ExecutableArgs>() -> impl std::future::Future<Output=()> {
    let (sender, receiver) = mpsc::channel(1);
    tokio::spawn(listen_for_commands_inner::<P>(receiver));
    async move {
        let _sender = sender;
        std::future::pending::<()>().await;
    }
}


async fn listen_for_commands_inner<P: ExecutableArgs + Send>(mut receiver: mpsc::Receiver<()>) {
    #[cfg(unix)]
    let _ = std::fs::remove_file(get_socket_name());

    let listener = LocalSocketListener::bind(get_socket_name())
        .expect("Command listener should have started successfully");

    loop {
        let mut stream;

        macro_rules! unwrap {
            ($result: expr) => {
                match $result {
                    Ok(x) => x,
                    Err(e) => {
                        error!("Faced the following error while listening for commands: {e}");
                        // let _ = send_msg(BaseCommand::Packet(e.to_string()), &mut stream).await;
                        continue;
                    }
                }
            };
        }

        tokio::select! {
            _ = receiver.recv() => {
                break
            }
            result = listener.accept() => {
                stream = unwrap!(result);
            }
        }

        let msg: BaseCommand = unwrap!(recv_msg(&mut stream).await);

        match msg {
            BaseCommand::IdRequest => {
                unwrap!(send_msg(BaseCommand::IdResponse(std::process::id()), &mut stream).await);
            }
            BaseCommand::Args(args) => {
                let args = match P::try_parse_from(args) {
                    Ok(x) => x,
                    Err(e) => {
                        unwrap!(send_msg(BaseCommand::Packet(e.to_string()), &mut stream).await);
                        let _ = stream.close().await;
                        continue;
                    }
                };
                if args
                    .execute(RemoteClient {
                        stream: Some(stream),
                    })
                    .await
                {
                    break;
                }
                continue;
            }
            _ => {}
        }

        unwrap!(send_msg(BaseCommand::CloseSocket, &mut stream).await);
    }
}
