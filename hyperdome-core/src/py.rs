use std::{
    fs::read_to_string,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use axum::{
    body::Bytes, extract::WebSocketUpgrade, http::StatusCode, response::{IntoResponse, Response}, routing::get,
    Router,
};
use fxhash::FxHashMap;
use parking_lot::RwLock;
use pyo3::{
    intern,
    types::PyModule,
    PyObject, Python, ToPyObject,
};

use crate::u16_to_status;

// #[cfg(feature = "hot-reload")]
#[derive(Default, Clone)]
struct PyHandlers {
    get: Option<PyObject>,
    post: Option<PyObject>,
    ws: Option<PyObject>,
    is_multi_pathed: bool,
}

#[cfg(feature = "hot-reload")]
static PY_HANDLERS: OnceLock<RwLock<FxHashMap<PathBuf, PyHandlers>>> = OnceLock::new();

fn load_py_handlers(path: &Path) -> Option<PyHandlers> {
    Python::with_gil(|py| {
        let module = PyModule::from_code(
            py,
            &read_to_string(path).expect("Script should be readable"),
            path.file_name()?
                .to_str()
                .expect("Script filename should be valid unicode"),
            path.file_prefix()?
                .to_str()
                .expect("Script filename should be valid unicode"),
        )
        .expect("Python Script should be readable and valid");

        let is_multi_pathed = module
            .getattr(intern!(py, "IS_MULTI_PATHED"))
            .map(|x| x.is_true())
            .flatten()
            .unwrap_or_default();

        let get_name = intern!(py, "get_handler");
        let post_name = intern!(py, "post_handler");

        let has_get = module
            .hasattr(get_name)
            .expect("Should be able to check the existence of get_handler");
        let has_post = module
            .hasattr(post_name)
            .expect("Should be able to check the existence of post_handler");

        if let Ok(ws_handler) = module.getattr(intern!(py, "ws_handler")) {
            if has_get || has_post {
                panic!("{path:?} contains both websocket handlers and get/post handlers");
            }

            Some(PyHandlers {
                ws: Some(ws_handler.to_object(py)),
                is_multi_pathed,
                ..Default::default()
            })
        } else {
            let get = has_get.then(|| {
                module
                    .getattr(get_name)
                    .expect("get_handler should be accessible since it exists")
                    .to_object(py)
            });
            let post = has_get.then(|| {
                module
                    .getattr(post_name)
                    .expect("post_handler should be accessible since it exists")
                    .to_object(py)
            });

            let mut py_handlers = PyHandlers::default();
            py_handlers.is_multi_pathed = is_multi_pathed;

            if let Some(get) = get {
                py_handlers.get = Some(get.to_object(py))
            }
            if let Some(post) = post {
                py_handlers.post = Some(post.to_object(py))
            }
            Some(py_handlers)
        }
    })
}

fn pyobject_to_response(py: Python, obj: PyObject, handler: &str) -> Response {
    if let Ok((code, bytes)) = obj.extract::<(u16, Vec<u8>)>(py) {
        (
            u16_to_status(code, || format!("{handler} should return a valid status code, not {code}")),
            bytes
        ).into_response()
    } else if let Ok((code, string)) = obj.extract::<(u16, String)>(py) {
        (u16_to_status(code, || format!("{handler} should return a valid status code, not {code}")), string).into_response()
    } else {
        panic!("{handler} should return a tuple: (Status Code, string/bytes), not: {obj}")
    }
}

pub(crate) fn load_py_into_router(mut router: Router, path: &Path) -> Router {
    let Some(py_handlers) = load_py_handlers(path) else {
        return router;
    };

    let http_path = {
        let path = path
            .to_str()
            .expect("Path to scripts should be valid unicode")
            .to_owned();

        if py_handlers.is_multi_pathed {
            path + "*"
        } else {
            path
        }
    };

    #[cfg(feature = "hot-reload")]
    {
        macro_rules! handler {
            ($method: ident) => {
                if py_handlers.$method.is_some() {
                    let path = path.to_owned();
                    router = router.route(&http_path, axum::routing::get(
                        move |body: Bytes| async move {
                            Python::with_gil(|py| {
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
                                    .$method
                                    .as_ref()
                                    .unwrap()
                                    .call1(py, (body,))
                                    .expect("get_handler should have ran without exceptions");
        
                                pyobject_to_response(py, result, "get_handler")
                            })
                        }
                    ));
                }
            };
        }

        handler!(get);
        handler!(post);

        
        if py_handlers.ws.is_some() {
            let path = path.to_owned();
            router = router.route(&http_path, axum::routing::get(
                move |ws: WebSocketUpgrade| async move {
                    Python::with_gil(|py| {
                        let result = PY_HANDLERS
                            .get()
                            .unwrap()
                            .read()
                            .get(&path)
                            .unwrap()
                            .ws
                            .as_ref()
                            .unwrap()
                            .call1(py, ())
                            .expect("ws_handler should have ran without exceptions");

                        if result.is_none(py) {
                            ws.on_upgrade(|ws| {
                                
                            })
                        }
                    })
                }
            ));
        }

        PY_HANDLERS
            .get_or_init(Default::default)
            .write()
            .insert(path.to_owned(), py_handlers);
    }

    router
}

#[cfg(feature = "hot-reload")]
pub(crate) fn py_handle_notify_event(event: &notify::Event) {
    use log::warn;

    let Some(mut lock) = PY_HANDLERS.get().map(RwLock::write) else {
        return;
    };

    for path in &event.paths {
        lock.get_mut(path)
            .map(|py_handler| {
                let new_py_handler = load_py_handlers(&path).unwrap();
                if new_py_handler.is_multi_pathed != py_handler.is_multi_pathed {
                    warn!("The IS_MULTI_PATHED constant in {path:?} has changed, but the server must be restarted for this change to be reflected");
                }
                if let Some(new_get) = new_py_handler.get {
                    if let Some(old_get) = &mut py_handler.get {
                        *old_get = new_get;
                    } else {
                        warn!("get_handler has been removed from {path:?}, but the server must be restarted for this change to be reflected");
                    }
                }
                if let Some(new_post) = new_py_handler.post {
                    if let Some(old_post) = &mut py_handler.post {
                        *old_post = new_post;
                    } else {
                        warn!("post_handler has been removed from {path:?}, but the server must be restarted for this change to be reflected");
                    }
                }
                if let Some(new_ws) = new_py_handler.ws {
                    if let Some(old_ws) = &mut py_handler.ws {
                        *old_ws = new_ws;
                    } else {
                        warn!("ws_handler has been removed from {path:?}, but the server must be restarted for this change to be reflected");
                    }
                }
            });
    }
}
