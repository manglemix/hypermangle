#![feature(path_file_prefix)]
#![feature(result_flattening)]

use std::{
    ffi::OsStr,
    fs::read_to_string,
    net::SocketAddr,
    path::Path,
    sync::{Arc, OnceLock},
    time::SystemTime,
};

use axum::{http::StatusCode, Router};
use bearer::BearerAuth;
use log::error;
use py::load_py_into_router;
use pyo3_asyncio::TaskLocals;
use regex::RegexSet;
use serde::Deserialize;
use tokio::runtime::Handle;
use tower::ServiceBuilder;
use tower_http::{
    auth::AsyncRequireAuthorizationLayer, compression::CompressionLayer, cors::CorsLayer,
    trace::TraceLayer,
};

pub use pyo3_asyncio::{self, tokio::main as hypermangle_main};
pub use pyo3::{self, PyResult};

mod bearer;
mod py;

#[cfg(feature = "hot-reload")]
const SYNC_CHANGES_DELAY: std::time::Duration = std::time::Duration::from_millis(1000);

static PY_TASK_LOCALS: OnceLock<TaskLocals> = OnceLock::new();

pub fn load_scripts_into_router(mut router: Router, path: &Path, async_runtime: Handle) -> Router {
    #[cfg(feature = "hot-reload")]
    {
        use notify::Watcher;
        let async_runtime = async_runtime.clone();
        let working_dir = path.canonicalize().unwrap().parent().unwrap().to_owned();
        let mut watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, _>| match res {
                Ok(event) => {
                    let _guard = async_runtime.enter();
                    let event = Arc::new(event);
                    py::py_handle_notify_event(event.clone(), working_dir.clone());
                }
                Err(event) => error!("File Watcher Error: {event:?}"),
            })
            .expect("Filesystem notification should be available");

        watcher
            .watch(path, notify::RecursiveMode::Recursive)
            .expect("Scripts folder should be watchable");

        Box::leak(Box::new(watcher));
    }

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
            router = load_scripts_into_router(router, &path, async_runtime.clone());
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


pub async fn auto_main(router: impl FnOnce(Router) -> Router) {
    let config = HyperDomeConfig::from_toml_file("hypermangle.toml".as_ref());
    setup_logger();
    async_run_router(
        load_scripts_into_router(router(Router::new()), "scripts".as_ref(), Handle::current()),
        config,
    )
    .await;
}
