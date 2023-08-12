#![feature(exclusive_wrapper)]

use std::mem::replace;
use std::ops::Deref;
use std::ops::DerefMut;
use std::sync::Arc;

use axum::extract::ws::Message;
use axum::extract::WebSocketUpgrade;
use axum::response::Response;
use pyo3::create_exception;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use tokio::sync::Mutex;

create_exception!(hypermangle_py, ClosedWebSocket, pyo3::exceptions::PyException);
create_exception!(hypermangle_py, WebSocketError, pyo3::exceptions::PyException);
create_exception!(hypermangle_py, NotYetAccepted, pyo3::exceptions::PyException);
create_exception!(hypermangle_py, AlreadyAccepted, pyo3::exceptions::PyException);

enum WebSocketInner {
    Pending((WebSocketUpgrade, tokio::sync::oneshot::Sender<Response>)),
    Accepting,
    Accepted(axum::extract::ws::WebSocket),
}

#[pyclass(frozen)]
pub struct WebSocket {
    inner: Arc<Mutex<WebSocketInner>>,
}

#[pyclass(frozen)]
struct WebSocketMessage {
    msg: Message,
}

#[pymethods]
impl WebSocketMessage {
    fn as_string(&self) -> Option<&str> {
        match &self.msg {
            Message::Text(msg) => Some(msg),
            _ => None,
        }
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        match &self.msg {
            Message::Binary(msg) => Some(msg),
            _ => None,
        }
    }
}

#[pymethods]
impl WebSocket {
    fn accept(&self) -> PyResult<()> {
        let mut lock = self.inner.clone().blocking_lock_owned();

        if let WebSocketInner::Pending(_) = lock.deref() {
            // Should be in this state
        } else {
            return Err(AlreadyAccepted::new_err(()));
        }

        let WebSocketInner::Pending((ws, sender)) =
            replace(lock.deref_mut(), WebSocketInner::Accepting)
        else {
            unreachable!()
        };

        sender
            .send(ws.on_upgrade(move |ws| async move {
                *lock = WebSocketInner::Accepted(ws);
            }))
            .expect("WebSocket Response Receiver should not have been dropped yet");

        Ok(())
    }

    fn recv_msg<'a>(&self, py: Python<'a>) -> PyResult<&'a PyAny> {
        let inner = self.inner.clone();

        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut lock = inner.lock().await;
            let WebSocketInner::Accepted(ws) = lock.deref_mut() else {
                return Err(NotYetAccepted::new_err(()));
            };
            let Some(result) = ws.recv().await else {
                return Err(ClosedWebSocket::new_err(()));
            };

            match result {
                Ok(msg) => Ok(WebSocketMessage { msg }),
                Err(e) => Err(WebSocketError::new_err(e.to_string())),
            }
        })
    }

    fn send_msg<'a>(&self, py: Python<'a>, msg: &'a PyAny) -> PyResult<&'a PyAny> {
        let msg = if let Ok(msg) = msg.extract::<String>() {
            Message::Text(msg)
        } else if let Ok(msg) = msg.extract::<Vec<u8>>() {
            Message::Binary(msg)
        } else {
            return Err(PyValueError::new_err(
                "WebSockets can only send Strings or Bytes",
            ));
        };
        let inner = self.inner.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            let mut lock = inner.lock().await;
            let WebSocketInner::Accepted(ws) = lock.deref_mut() else {
                return Err(NotYetAccepted::new_err(()));
            };
            ws.send(msg)
                .await
                .map_err(|e| WebSocketError::new_err(e.to_string()))
        })
    }
}

impl WebSocket {
    pub fn new(ws: WebSocketUpgrade) -> (Self, tokio::sync::oneshot::Receiver<Response>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (
            Self {
                inner: Arc::new(Mutex::new(WebSocketInner::Pending((ws, sender)))),
            },
            receiver,
        )
    }
}

#[pymodule]
fn hypermangle_py(py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add("ClosedWebSocket", py.get_type::<ClosedWebSocket>())?;
    m.add("WebSocketError", py.get_type::<WebSocketError>())?;
    m.add("NotYetAccepted", py.get_type::<NotYetAccepted>())?;
    m.add("AlreadyAccepted", py.get_type::<AlreadyAccepted>())?;
    m.add_class::<WebSocket>()?;
    m.add_class::<WebSocketMessage>()?;
    Ok(())
}
