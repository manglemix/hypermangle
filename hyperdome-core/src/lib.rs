#![feature(path_file_prefix)]
#![feature(result_flattening)]

use std::{
    ffi::OsStr, fs::read_to_string, net::SocketAddr, path::Path, sync::OnceLock, time::SystemTime,
};

use axum::{http::StatusCode, Router};
use bearer::BearerAuth;
use log::error;
use py::load_py_into_router;
use pyo3_asyncio::TaskLocals;
use regex::RegexSet;
use serde::Deserialize;
use tower::ServiceBuilder;
use tower_http::{
    auth::AsyncRequireAuthorizationLayer, compression::CompressionLayer, cors::CorsLayer,
    trace::TraceLayer,
};

mod bearer;
mod py;

#[cfg(feature = "hot-reload")]
static FILE_WATCHER: OnceLock<notify::RecommendedWatcher> = OnceLock::new();

static PY_TASK_LOCALS: OnceLock<TaskLocals> = OnceLock::new();

pub fn load_scripts_into_router(mut router: Router, path: &Path) -> Router {
    #[cfg(feature = "hot-reload")]
    let _ = FILE_WATCHER.set(
        notify::recommended_watcher(|res| match res {
            Ok(event) => {
                py::py_handle_notify_event(&event);
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
        let path = entry.path();
        let file_type = entry
            .file_type()
            .expect("File type of script or sub-directory should be accessible");

        if file_type.is_dir() {
            router = load_scripts_into_router(router, &path);
        } else if file_type.is_file() {
            match path.extension().map(OsStr::to_str).flatten() {
                Some("py") => router = load_py_into_router(router, &path),
                _ => {}
            }
        } else {
            panic!("Failed to get the file type of {entry:?}");
        }
    }

    router
}

pub fn setup_logger() {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                humantime::format_rfc3339_seconds(SystemTime::now()),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log::LevelFilter::Info)
        .chain(std::io::stdout())
        .apply()
        .expect("Logger should initialize successfully");
}

// #[pyo3_asyncio::tokio::main(flavor = "multi_thread")]
// pub fn run_router(router: Router, config: HyperDomeConfig) {
//     let mut builder = tokio::runtime::Builder::new_multi_thread();
//     builder.enable_all();
//     pyo3_asyncio::tokio::init(builder);
//     PY_TASK_LOCALS.set(pyo3::Python::with_gil(|py| pyo3_asyncio::tokio::get_current_locals(py)).unwrap()).unwrap();
//     // pyo3::Python::with_gil(|py| {
//     //     let asyncio = py.import("asyncio").unwrap();
//     //     let event_loop = asyncio.call_method0("new_event_loop").expect("Python asyncio.new_event_loop should have ran without error");
//     //     // asyncio.call_method1("set_event_loop", (event_loop,)).expect("Python asyncio.set_event_loop should have ran without error");
//     //     PY_TASK_LOCALS.set(TaskLocals::new(event_loop)).unwrap();
//     // });
//     pyo3_asyncio::tokio::get_runtime().
//         block_on(async_run_router(router, config));
// }

#[inline]
fn u16_to_status(code: u16, f: impl Fn() -> String) -> StatusCode {
    StatusCode::from_u16(code).expect(&f())
}

#[derive(Deserialize)]
pub struct HyperDomeConfig {
    #[serde(default)]
    cors_methods: Vec<String>,
    #[serde(default)]
    cors_origins: Vec<String>,
    #[serde(default)]
    api_token: String,
    bind_address: SocketAddr,
    #[serde(default)]
    public_paths: Vec<String>,
}

impl HyperDomeConfig {
    pub fn from_toml_file(path: &Path) -> Self {
        let txt = read_to_string(path).expect(&format!("{path:?} should be readable"));
        toml::from_str(&txt).expect(&format!("{path:?} should be valid toml"))
    }
}

#[inline]
pub async fn async_run_router(mut router: Router, config: HyperDomeConfig) {
    PY_TASK_LOCALS
        .set(pyo3::Python::with_gil(|py| pyo3_asyncio::tokio::get_current_locals(py)).unwrap())
        .unwrap();

    router = router.layer(
        ServiceBuilder::new()
            .layer(CompressionLayer::new())
            .layer(TraceLayer::new_for_http())
            .layer(
                CorsLayer::new()
                    .allow_methods(
                        config
                            .cors_methods
                            .into_iter()
                            .map(|x| {
                                x.parse()
                                    .expect("CORS Method should be a valid HTTP Method")
                            })
                            .collect::<Vec<_>>(),
                    )
                    .allow_origin(
                        config
                            .cors_origins
                            .into_iter()
                            .map(|x| x.parse().expect("CORS Origin should be a valid origin"))
                            .collect::<Vec<_>>(),
                    ),
            ),
    );

    if !config.api_token.is_empty() {
        router = router.layer(AsyncRequireAuthorizationLayer::new(BearerAuth::new(
            config.api_token.parse().expect("msg"),
            RegexSet::new(config.public_paths).expect("msg"),
        )));
    }

    axum::Server::bind(&config.bind_address)
        .serve(router.into_make_service())
        .await
        .unwrap();
}
