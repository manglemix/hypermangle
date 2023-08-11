#![feature(exclusive_wrapper)]

use std::sync::Arc;

use axum::extract::ws::Message;
use pyo3::prelude::*;
use pyo3::create_exception;
use tokio::sync::Mutex;

create_exception!(hyperdome_py, ClosedWebSocket, pyo3::exceptions::PyException);
create_exception!(hyperdome_py, WebSocketError, pyo3::exceptions::PyException);


#[pyclass]
struct WebSocket {
    ws: Arc<Mutex<axum::extract::ws::WebSocket>>
}


#[pyclass]
struct WebSocketMessage {
    msg: Message
}

#[pymethods]
impl WebSocketMessage {
    fn as_string(&self) -> Option<&str> {
        match &self.msg {
            Message::Text(msg) => Some(msg),
            _ => None
        }
    }

    fn as_bytes(&self) -> Option<&[u8]> {
        match &self.msg {
            Message::Binary(msg) => Some(msg),
            _ => None
        }
    }
}

#[pymethods]
impl WebSocket {
    fn recv_msg<'a>(&self, py: Python<'a>) -> PyResult<&'a PyAny> {
        let ws = self.ws.clone();
        pyo3_asyncio::tokio::future_into_py(py, async move {
            loop {
                let Some(result) = ws.lock().await.recv().await else { return Err(ClosedWebSocket::new_err(())) };
                break match result {
                    Ok(msg) => Ok(WebSocketMessage { msg }),
                    Err(e) => return Err(WebSocketError::new_err(e.to_string()))
                }
            }
        })
    }
}


#[pymodule]
fn hyperdome_py(py: Python<'_>, m: &PyModule) -> PyResult<()> {
    m.add("ClosedWebSocket", py.get_type::<ClosedWebSocket>())?;
    m.add("WebSocketError", py.get_type::<WebSocketError>())?;
    m.add_class::<WebSocket>()?;
    m.add_class::<WebSocketMessage>()?;
    Ok(())
}