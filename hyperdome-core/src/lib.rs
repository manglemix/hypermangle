#![feature(path_file_prefix)]
#![feature(result_flattening)]

use std::{ffi::OsStr, net::SocketAddr, path::Path, sync::OnceLock};

use axum::{http::StatusCode, Router};
use bearer::BearerAuth;
use log::error;
use py::load_py_into_router;
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
pub async fn run_router(router: Router, config: HyperDomeConfig) {
    async_run_router(router, config).await;
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

#[inline]
pub async fn async_run_router(mut router: Router, config: HyperDomeConfig) {
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
