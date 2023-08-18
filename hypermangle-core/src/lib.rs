#![feature(path_file_prefix)]
#![feature(result_flattening)]
#![feature(never_type)]

use std::{
    error::Error,
    ffi::OsStr,
    fs::{read_to_string, File},
    io::BufReader,
    net::SocketAddr,
    path::Path,
    time::SystemTime,
};

use axum::Router;
use bearer::BearerAuth;
use hyper::server::{accept::Accept, Builder};
use lers::solver::Http01Solver;
use log::{info, warn};
#[cfg(feature = "python")]
use py::load_py_into_router;
#[cfg(feature = "python")]
use pyo3_asyncio::TaskLocals;
use regex::RegexSet;
use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    runtime::Handle,
};
use tokio_rustls::rustls::{Certificate, PrivateKey};
use tower::ServiceBuilder;
use tower_http::{
    auth::AsyncRequireAuthorizationLayer, compression::CompressionLayer, cors::CorsLayer,
    trace::TraceLayer,
};

// pub use axum;
// #[cfg(feature = "python")]
// pub use pyo3::{self, PyResult};
// #[cfg(feature = "python")]
// pub use pyo3_asyncio::{self, tokio::main as hypermangle_main};

use crate::tls::TlsAcceptor;

mod bearer;
#[cfg(feature = "python")]
mod py;
mod tls;

#[cfg(all(feature = "hot-reload", feature = "python"))]
const SYNC_CHANGES_DELAY: std::time::Duration = std::time::Duration::from_millis(1000);

#[cfg(feature = "python")]
static PY_TASK_LOCALS: std::sync::OnceLock<TaskLocals> = std::sync::OnceLock::new();

pub fn load_scripts_into_router(mut router: Router, path: &Path) -> Router {
    let async_runtime = Handle::current();

    #[cfg(feature = "python")]
    {
        #[cfg(feature = "hot-reload")]
        {
            use notify::Watcher;
            let async_runtime = async_runtime.clone();
            let working_dir = path.canonicalize().unwrap().parent().unwrap().to_owned();
            let mut watcher =
                notify::recommended_watcher(move |res: Result<notify::Event, _>| match res {
                    Ok(event) => {
                        let _guard = async_runtime.enter();
                        let event = std::sync::Arc::new(event);
                        py::py_handle_notify_event(event.clone(), working_dir.clone());
                    }
                    Err(event) => log::error!("File Watcher Error: {event:?}"),
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
                router = load_scripts_into_router(router, &path);
            } else if file_type.is_file() {
                match path.extension().map(OsStr::to_str).flatten() {
                    #[cfg(feature = "python")]
                    Some("py") => router = load_py_into_router(router, &path),
                    _ => {}
                }
            } else {
                panic!("Failed to get the file type of {entry:?}");
            }
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

#[cfg(feature = "python")]
#[inline]
fn u16_to_status(code: u16, f: impl Fn() -> String) -> axum::http::StatusCode {
    axum::http::StatusCode::from_u16(code).expect(&f())
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
    #[serde(default)]
    cert_path: String,
    #[serde(default)]
    key_path: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    domain_name: String,
}

impl HyperDomeConfig {
    pub fn from_toml_file(path: &Path) -> Self {
        let txt = read_to_string(path).expect(&format!("{path:?} should be readable"));
        toml::from_str(&txt).expect(&format!("{path:?} should be valid toml"))
    }
}

#[inline]
pub async fn async_run_router<I>(server: Builder<I>, mut router: Router, config: HyperDomeConfig)
where
    I: Accept,
    I::Error: Into<Box<dyn Error + Send + Sync>>,
    I::Conn: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    router = load_scripts_into_router(router, "scripts".as_ref());

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

    server.serve(router.into_make_service()).await.unwrap();
}

#[tokio::main]
pub async fn auto_main(router: Router) {
    let config = HyperDomeConfig::from_toml_file("hypermangle.toml".as_ref());
    setup_logger();

    #[cfg(feature = "python")]
    PY_TASK_LOCALS
        .set(pyo3::Python::with_gil(|py| pyo3_asyncio::TaskLocals::new(py.import("asyncio").unwrap().call_method0("new_event_loop").unwrap())))
        .unwrap();

    if !config.cert_path.is_empty() && !config.key_path.is_empty() {
        let cert_path: &Path = config.cert_path.as_ref();
        let key_path: &Path = config.key_path.as_ref();

        if cert_path.exists() && key_path.exists() {
            info!("Loading HTTP Certificates");
            let file = File::open(cert_path).expect("Cert path should be readable");
            let mut reader = BufReader::new(file);
            let certs = rustls_pemfile::certs(&mut reader).expect("Cert file should be valid");
            let certs: Vec<_> = certs.into_iter().map(Certificate).collect();

            let file = File::open(&key_path).expect("Key path should be readable");
            let mut reader = BufReader::new(file);
            let mut keys =
                rustls_pemfile::pkcs8_private_keys(&mut reader).expect("Key file should be valid");

            let key = match keys.len() {
                0 => panic!("No PKCS8-encoded private key found in key file"),
                1 => PrivateKey(keys.remove(0)),
                _ => panic!("More than one PKCS8-encoded private key found in key file"),
            };

            async_run_router(
                axum::Server::builder(TlsAcceptor::new(certs, key, &config.bind_address).await),
                router,
                config,
            )
            .await;
            return;
        } else if !cert_path.exists() && !key_path.exists() {
            warn!("Acquiring HTTP Certificates");
            macro_rules! unwrap {
                ($result: expr) => {
                    match $result {
                        Ok(x) => x,
                        Err(e) => {
                            panic!("Error running LERS: {e}");
                        }
                    }
                };
            }

            #[cfg(not(debug_assertions))]
            const URL: &str = lers::LETS_ENCRYPT_PRODUCTION_URL;
            #[cfg(debug_assertions)]
            const URL: &str = lers::LETS_ENCRYPT_STAGING_URL;

            if config.email.is_empty() {
                panic!("Email not provided!");
            }

            let solver = Http01Solver::new();
            let handle = unwrap!(solver.start(&config.bind_address));

            let directory = unwrap!(
                lers::Directory::builder(URL)
                    .http01_solver(Box::new(solver))
                    .build()
                    .await
            );

            let account = unwrap!(
                directory
                    .account()
                    .terms_of_service_agreed(true)
                    .contacts(vec![format!("mailto:{}", config.email)])
                    .create_if_not_exists()
                    .await
            );

            let certificate = unwrap!(
                account
                    .certificate()
                    .add_domain(&config.domain_name)
                    .obtain()
                    .await
            );

            tokio::spawn(handle.stop());

            let certs: Vec<_> = certificate
                .x509_chain()
                .iter()
                .map(|x| Certificate(x.to_der().unwrap()))
                .collect();
            let key = PrivateKey(certificate.private_key_to_der().unwrap());

            let bind_address = config.bind_address.clone();
            async_run_router(
                axum::Server::builder(TlsAcceptor::new(certs, key, &bind_address).await),
                router,
                config,
            )
            .await;
            return;
        } else if !cert_path.exists() {
            panic!("Certificate does not exist at the given path");
        } else {
            panic!("Private Key does not exist at the given path");
        }
    }

    async_run_router(
        axum::Server::bind(&config.bind_address),
        router,
        config,
    ).await;
}
