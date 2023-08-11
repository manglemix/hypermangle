#![feature(path_file_prefix)]
#![feature(result_flattening)]

use std::{
    ffi::OsStr,
    fs::read_to_string,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use axum::{http::StatusCode, routing::MethodFilter, Router};
use bearer::BearerAuth;
use fxhash::FxHashMap;
use log::error;
use notify::RecommendedWatcher;
use py::{load_py_into_router, py_handle_notify_event};
use pyo3::{intern, types::PyModule, PyObject, Python, ToPyObject};
use tower::ServiceBuilder;
use tower_http::{
    auth::AsyncRequireAuthorizationLayer, compression::CompressionLayer, cors::CorsLayer,
    trace::TraceLayer,
};

mod bearer;
mod py;

enum HTTPHandler<A, B, C> {
    Requests(RequestHandler<A, B>),
    WebSocket(C),
}

enum RequestHandler<A, B> {
    Get(A),
    Post(B),
    GetAndPost(A, B),
}

impl<A, B> RequestHandler<A, B> {
    fn get_get(&self) -> Option<&A> {
        match self {
            RequestHandler::Get(x) => Some(x),
            RequestHandler::Post(_) => None,
            RequestHandler::GetAndPost(x, _) => Some(x),
        }
    }

    fn get_post(&self) -> Option<&B> {
        match self {
            RequestHandler::Get(_) => None,
            RequestHandler::Post(x) => Some(x),
            RequestHandler::GetAndPost(_, x) => Some(x),
        }
    }
}

// enum Script {
//     Py {
//         handler: HTTPHandler<PyObject, PyObject, PyObject>,
//         is_multi_pathed: bool,
//     },
// }

// impl Script {
//     fn load_from_file(path: &Path) -> Option<Self> {
//         match path.extension()?.to_str()? {
//             "py" => Python::with_gil(|py| {
//                 let module = PyModule::from_code(
//                     py,
//                     &read_to_string(path).expect("Script should be readable"),
//                     path.file_name()?
//                         .to_str()
//                         .expect("Script filename should be valid unicode"),
//                     path.file_prefix()?
//                         .to_str()
//                         .expect("Script filename should be valid unicode"),
//                 )
//                 .expect("Python Script should be readable and valid");

//                 let get_name = intern!(py, "get_handler");
//                 let post_name = intern!(py, "post_handler");

//                 let has_get = module
//                     .hasattr(get_name)
//                     .expect("Should be able to check the existence of get_handler");
//                 let has_post = module
//                     .hasattr(post_name)
//                     .expect("Should be able to check the existence of post_handler");

//                 let handler = if let Ok(ws_handler) = module.getattr(intern!(py, "ws_handler")) {
//                     if has_get || has_post {
//                         panic!(
//                             "{path:?} contains both websocket handlers and get or post handlers"
//                         );
//                     }
//                     HTTPHandler::WebSocket(ws_handler.to_object(py))
//                 } else {
//                     let get = has_get.then(|| {
//                         module
//                             .getattr(get_name)
//                             .expect("get_handler should be accessible since it exists")
//                             .to_object(py)
//                     });
//                     let post = has_get.then(|| {
//                         module
//                             .getattr(post_name)
//                             .expect("post_handler should be accessible since it exists")
//                             .to_object(py)
//                     });

//                     if let Some(get) = get {
//                         if let Some(post) = post {
//                             HTTPHandler::Requests(RequestHandler::GetAndPost(get, post))
//                         } else {
//                             HTTPHandler::Requests(RequestHandler::Get(get))
//                         }
//                     } else if let Some(post) = post {
//                         HTTPHandler::Requests(RequestHandler::Post(post))
//                     } else {
//                         return None;
//                     }
//                 };

//                 Some(Script::Py {
//                     handler,
//                     is_multi_pathed: module
//                         .getattr(intern!(py, "IS_MULTI_PATHED"))
//                         .map(|x| x.is_true())
//                         .flatten()
//                         .unwrap_or_default(),
//                 })
//             }),
//             _ => None,
//         }
//     }

//     fn is_multi_pathed(&self) -> bool {
//         match self {
//             Script::Py {
//                 is_multi_pathed, ..
//             } => *is_multi_pathed,
//         }
//     }
// }

// enum MapOrObject<T> {
//     Map(PathMap<T>),
//     Object(T),
// }

// struct PathMap<T> {
//     map: FxHashMap<PathBuf, MapOrObject<T>>,
// }

// impl<T> PathMap<T> {
//     fn get_object(&self, path: &Path) -> Option<&T> {
//         let mut components = path.components();
//         self.map
//             .get(AsRef::<Path>::as_ref(&components.next()?))
//             .map(|x| match x {
//                 MapOrObject::Map(map) => map.get_object(components.as_path()),
//                 MapOrObject::Object(x) => Some(x),
//             })
//             .flatten()
//     }
// }

// struct Scripts {
//     mono_pathed: FxHashMap<String, Script>,
//     multi_pathed: PathMap<Script>,
// }

// fn load_script_into_router(router: Router, path: &Path) -> Router {
//     match path.extension().map(OsStr::to_str).flatten() {
//         Some("py") => load_py_into_router(router, path),
//         _ => router
//     }
// }

#[cfg(feature = "hot-reload")]
static FILE_WATCHER: OnceLock<RecommendedWatcher> = OnceLock::new();

pub fn load_scripts_into_router(mut router: Router, path: &Path) -> Router {
    #[cfg(feature = "hot-reload")]
    FILE_WATCHER.set(
        notify::recommended_watcher(|res| match res {
            Ok(event) => {
                py_handle_notify_event(&event);
            }
            Err(event) => error!("File Watcher Error: {event:?}"),
        })
        .expect("Filesystem notification should be available"),
    );

    for result in path
        .read_dir()
        .expect("Scripts directory should be readable")
    {
        let entry = result.expect("Script or sub-directory should be readable");
        let file_type = entry
            .file_type()
            .expect("File type of script or sub-directory should be accessible");

        if file_type.is_dir() {
            router = load_scripts_into_router(router, &entry.path());
        } else if file_type.is_file() {
            match path.extension().map(OsStr::to_str).flatten() {
                Some("py") => router = load_py_into_router(router, path),
                _ => {}
            }
        } else {
            panic!("Failed to get the file type of {entry:?}");
        }
    }

    router
}

#[tokio::main(flavor = "multi_thread")]
pub async fn run_router(router: Router) {
    async_run_router(router).await;
}

fn u16_to_status(code: u16, f: impl Fn() -> String) -> StatusCode {
    StatusCode::from_u16(code).expect(&f())
}

pub async fn async_run_router(router: Router) {
    // let router = router.layer(
    //     ServiceBuilder::new()
    //         .layer(CompressionLayer::new())
    //         .layer(TraceLayer::new_for_http())
    //         .layer(
    //             CorsLayer::new()
    //                 .allow_methods(cors_methods.clone())
    //                 .allow_origin(cors_origins.clone()),
    //         ),
    // );

    // if let Some(api_token) = api_token.clone() {
    //     app = app.layer(AsyncRequireAuthorizationLayer::new(BearerAuth::new(
    //         api_token,
    //         public_paths.clone(),
    //     )));
    // }

    // axum::Server::bind(&bind_address)
    //     .serve(router.into_make_service())
    //     // .with_graceful_shutdown(async {
    //     //     Python::with_gil(|py| {
    //     //         let Ok(init) = module.getattr(py, "init") else {
    //     //             return;
    //     //         };
    //     //         if let Err(e) = init.call0(py) {
    //     //             eprintln!("Error calling init from Python program: {e:?}");
    //     //         }
    //     //     });
    //     //     file_change_receiver.recv().await;
    //     //     eprintln!("Reloading script\n")
    //     // })
    //     .await
    //     .unwrap();
}
