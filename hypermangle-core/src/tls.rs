use std::{
    net::SocketAddr,
    sync::Arc,
    task::{self, Poll},
};

use futures::{stream::FuturesUnordered, StreamExt};
use hyper::server::accept::Accept;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::{
    rustls::{Certificate, ServerConfig},
    server::TlsStream,
};

pub struct TlsAcceptor {
    acceptor: tokio_rustls::TlsAcceptor,
    listener: TcpListener,
    accepting: FuturesUnordered<tokio_rustls::Accept<TcpStream>>,
}

impl TlsAcceptor {
    pub async fn new(
        certs: Vec<Certificate>,
        key: tokio_rustls::rustls::PrivateKey,
        bind_address: &SocketAddr,
    ) -> Self {
        Self {
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(
                ServerConfig::builder()
                    .with_safe_defaults()
                    .with_no_client_auth()
                    .with_single_cert(certs, key)
                    .expect("Certificate and Key should be valid"),
            )),
            listener: TcpListener::bind(bind_address)
                .await
                .expect("TcpListener should be binded"),
            accepting: Default::default(),
        }
    }
}

impl Accept for TlsAcceptor {
    type Conn = TlsStream<TcpStream>;

    type Error = std::io::Error;

    fn poll_accept(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> Poll<Option<Result<Self::Conn, Self::Error>>> {
        if let Poll::Ready(result) = self.listener.poll_accept(cx) {
            let (stream, _) = result?;
            self.accepting.push(self.acceptor.accept(stream));
        };
        
        let Poll::Ready(Some(result)) = self.accepting.poll_next_unpin(cx) else {
            return Poll::Pending;
        };
        Poll::Ready(Some(result))
    }
}
