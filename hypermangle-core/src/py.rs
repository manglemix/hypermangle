use std::{
    fs::read_to_string,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use axum::{
    body::Bytes,
    extract::WebSocketUpgrade,
    http::StatusCode,
    response::{IntoResponse, Response},
    Router,
};
use fxhash::FxHashMap;
use parking_lot::RwLock;
use pyo3::{intern, types::PyModule, PyErr, PyObject, Python, ToPyObject};

use crate::{u16_to_status, PY_TASK_LOCALS};

#[derive(Default, Clone, Debug)]
struct PyHandlers {
    get: Option<PyObject>,
    post: Option<PyObject>,
    ws: Option<PyObject>,
    is_multi_pathed: bool,
}

#[cfg(feature = "hot-reload")]
static PY_HANDLERS: OnceLock<
    RwLock<FxHashMap<PathBuf, (PyHandlers, std::sync::atomic::AtomicU8)>>,
> = OnceLock::new();

#[derive(Debug)]
enum LoadPyErr {
    PyErr(PyErr),
    NotAScript,
    InterferingHandlers,
    ReadError(std::io::Error),
}

impl From<PyErr> for LoadPyErr {
    fn from(value: PyErr) -> Self {
        Self::PyErr(value)
    }
}

impl From<std::io::Error> for LoadPyErr {
    fn from(value: std::io::Error) -> Self {
        Self::ReadError(value)
    }
}

fn load_py_handlers(path: &Path) -> Result<PyHandlers, LoadPyErr> {
    Python::with_gil(|py| {
        let module = PyModule::from_code(
            py,
            &read_to_string(path)?,
            path.file_name()
                .ok_or(LoadPyErr::NotAScript)?
                .to_str()
                .expect("Script filename should be valid unicode"),
            path.file_prefix()
                .ok_or(LoadPyErr::NotAScript)?
                .to_str()
                .expect("Script filename should be valid unicode"),
        )?;

        let is_multi_pathed = module
            .getattr(intern!(py, "IS_MULTI_PATHED"))
            .map(|x| x.is_true())
            .flatten()
            .unwrap_or_default();

        let get_name = intern!(py, "get_handler");
        let post_name = intern!(py, "post_handler");

        let has_get = module.hasattr(get_name)?;
        let has_post = module.hasattr(post_name)?;

        if let Ok(ws_handler) = module.getattr(intern!(py, "ws_handler")) {
            if has_get || has_post {
                return Err(LoadPyErr::InterferingHandlers);
            }

            Ok(PyHandlers {
                ws: Some(ws_handler.to_object(py)),
                is_multi_pathed,
                ..Default::default()
            })
        } else {
            let get = if has_get {
                Some(module.getattr(get_name)?.to_object(py))
            } else {
                None
            };
            let post = if has_post {
                Some(module.getattr(post_name)?.to_object(py))
            } else {
                None
            };

            let mut py_handlers = PyHandlers::default();
            py_handlers.is_multi_pathed = is_multi_pathed;

            if let Some(get) = get {
                py_handlers.get = Some(get.to_object(py))
            }
            if let Some(post) = post {
                py_handlers.post = Some(post.to_object(py))
            }
            Ok(py_handlers)
        }
    })
}

fn pyobject_to_response<'a>(py: Python<'a>, obj: PyObject, handler: &str) -> Response {
    if let Ok((code, bytes)) = obj.extract::<(u16, Vec<u8>)>(py) {
        (
            u16_to_status(code, || {
                format!("{handler} should return a valid status code, not {code}")
            }),
            bytes,
        )
            .into_response()
    } else if let Ok((code, string)) = obj.extract::<(u16, String)>(py) {
        (
            u16_to_status(code, || {
                format!("{handler} should return a valid status code, not {code}")
            }),
            string,
        )
            .into_response()
    } else {
        panic!("{handler} should return a tuple: (Status Code, string/bytes), not: {obj}")
    }
}

pub(crate) fn load_py_into_router(mut router: Router, path: &Path) -> Router {
    let py_handlers = match load_py_handlers(path) {
        Ok(x) => x,
        Err(LoadPyErr::NotAScript) => return router,
        e => e.expect("Python Script should be valid"),
    };

    let http_path = {
        let mut components = path.components();
        // Skip over scripts folder
        components.next();

        let path = components
            .as_path()
            .parent()
            .unwrap()
            .to_str()
            .expect("Path to scripts should be valid unicode")
            .to_owned();

        String::from("/") + &path
    };

    #[cfg(feature = "hot-reload")]
    {
        macro_rules! handler {
            ($method: ident, $handler: literal) => {
                if py_handlers.$method.is_some() {
                    let path = path.to_owned();
                    let handler = axum::routing::$method(move |body: Bytes| async move {
                        let exception_msg =
                            format!("{} should have ran without exceptions", $handler);
                        let result = Python::with_gil(|py| {
                            let body = if let Ok(body) = std::str::from_utf8(&body) {
                                body.to_object(py)
                            } else {
                                body.to_object(py)
                            };

                            let result = PY_HANDLERS
                                .get()
                                .unwrap()
                                .read()
                                .get(&path)
                                .unwrap()
                                .0
                                .$method
                                .as_ref()
                                .unwrap()
                                .call1(py, (body,))
                                .expect(&exception_msg);

                            pyo3_asyncio::into_future_with_locals(
                                PY_TASK_LOCALS.get().unwrap(),
                                result.as_ref(py),
                            )
                            .expect(&format!("{} should be asynchronous", $handler))
                        })
                        .await
                        .expect(&exception_msg);

                        Python::with_gil(|py| pyobject_to_response(py, result, $handler))
                    });
                    router = router.route(&http_path, handler.clone());

                    if py_handlers.is_multi_pathed {
                        router = router.route(&format!("{http_path}*path"), handler);
                    }
                }
            };
        }

        handler!(get, "get_handler");
        handler!(post, "post_handler");

        if py_handlers.ws.is_some() {
            let path = path.to_owned();
            router = router.route(
                &http_path,
                axum::routing::get(|ws: WebSocketUpgrade| async move {
                    let (ws, receiver) = hypermangle_py::WebSocket::new(ws);

                    tokio::task::spawn_blocking(move || {
                        Python::with_gil(|py| {
                            PY_HANDLERS
                                .get()
                                .unwrap()
                                .read()
                                .get(&path)
                                .unwrap()
                                .0
                                .ws
                                .as_ref()
                                .unwrap()
                                .call1(py, (ws,))
                                .expect("ws_handler should have ran without exceptions");
                        })
                    });

                    receiver
                        .await
                        .unwrap_or_else(|_| (StatusCode::SERVICE_UNAVAILABLE, ()).into_response())
                }),
            );
        }

        PY_HANDLERS
            .get_or_init(Default::default)
            .write()
            .insert(path.to_owned(), (py_handlers, Default::default()));
    }

    router
}

#[cfg(feature = "hot-reload")]
pub(crate) fn py_handle_notify_event(
    event: std::sync::Arc<notify::Event>,
    working_directory: PathBuf,
) {
    use log::{error, info, warn};
    use parking_lot::RwLockUpgradableReadGuard;

    use crate::SYNC_CHANGES_DELAY;
    let Some(py_handlers) = PY_HANDLERS.get() else {
        return;
    };

    tokio::spawn(async move {
        for path in &event.paths {
            let path = path.canonicalize().unwrap();
            let Ok(path) = path.strip_prefix(&working_directory) else {
                continue;
            };

            let id = {
                let lock = py_handlers.read();
                let Some((_, instant)) = lock.get(path) else {
                    return;
                };
                instant.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
            };

            tokio::time::sleep(SYNC_CHANGES_DELAY).await;

            let lock = py_handlers.upgradable_read();

            if lock
                .get(path)
                .unwrap()
                .1
                .load(std::sync::atomic::Ordering::Relaxed)
                != id
            {
                return;
            }
            let mut lock = RwLockUpgradableReadGuard::upgrade(lock);
            let (py_handler, _) = lock.get_mut(path).unwrap();

            let new_py_handler = match load_py_handlers(&path) {
                Ok(x) => x,
                Err(e) => {
                    error!("Faced error while reloading {path:?}: {e:?}");
                    return;
                }
            };
            if new_py_handler.is_multi_pathed != py_handler.is_multi_pathed {
                warn!("The IS_MULTI_PATHED constant in {path:?} has changed, but the server must be restarted for this change to be reflected");
            }
            if let Some(new_get) = new_py_handler.get {
                if let Some(old_get) = &mut py_handler.get {
                    *old_get = new_get;
                } else {
                    warn!("get_handler has been added to {path:?}, but the server must be restarted for this change to be reflected");
                }
            } else if new_py_handler.get.is_some() {
                warn!("get_handler has been removed from {path:?}, but the server must be restarted for this change to be reflected");
            }
            if let Some(new_post) = new_py_handler.post {
                if let Some(old_post) = &mut py_handler.post {
                    *old_post = new_post;
                } else {
                    warn!("post_handler has been added to {path:?}, but the server must be restarted for this change to be reflected");
                }
            } else if new_py_handler.post.is_some() {
                warn!("post_handler has been removed from {path:?}, but the server must be restarted for this change to be reflected");
            }
            if let Some(new_ws) = new_py_handler.ws {
                if let Some(old_ws) = &mut py_handler.ws {
                    *old_ws = new_ws;
                } else {
                    warn!("ws_handler has been added to {path:?}, but the server must be restarted for this change to be reflected");
                }
            } else if new_py_handler.ws.is_some() {
                warn!("ws_handler has been removed from {path:?}, but the server must be restarted for this change to be reflected");
            }
            info!("Successfully reloaded {path:?}");
        }
    });
}
