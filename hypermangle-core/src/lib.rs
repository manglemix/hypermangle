#![feature(path_file_prefix)]
#![feature(result_flattening)]
#![feature(never_type)]
#![feature(os_str_bytes)]
#![feature(async_fn_in_trait)]

use std::{
    error::Error,
    fs::{read_to_string, write, File},
    io::BufReader,
    net::SocketAddr,
    path::Path,
    process::Stdio,
    time::SystemTime,
};

use axum::Router;
use bearer::BearerAuth;
use clap::{Parser, Subcommand};
use console::{listen_for_commands, send_args_to_remote, ExecutableArgs};
use hyper::server::{accept::Accept, Builder};
use lers::solver::Http01Solver;
use log::{info, warn};
#[cfg(feature = "python")]
use py::load_py_into_router;
#[cfg(feature = "python")]
use pyo3_asyncio::TaskLocals;
use regex::RegexSet;
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio_rustls::rustls::{Certificate, PrivateKey};
use tower::ServiceBuilder;
use tower_http::{
    auth::AsyncRequireAuthorizationLayer, compression::CompressionLayer, cors::CorsLayer,
    trace::TraceLayer,
};

use crate::{console::does_remote_exist, tls::TlsAcceptor};

mod bearer;
pub mod console;
#[cfg(feature = "python")]
mod py;
mod tls;

#[cfg(all(feature = "hot-reload", feature = "python"))]
const SYNC_CHANGES_DELAY: std::time::Duration = std::time::Duration::from_millis(1000);

#[cfg(feature = "python")]
static PY_TASK_LOCALS: std::sync::OnceLock<TaskLocals> = std::sync::OnceLock::new();

pub fn load_scripts_into_router(router: Router, path: &Path) -> Router {
    #[cfg(feature = "python")]
    {
        let mut router = router;
        #[cfg(feature = "hot-reload")]
        {
            use notify::Watcher;
            let async_runtime = tokio::runtime::Handle::current();
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
                match path.extension().map(std::ffi::OsStr::to_str).flatten() {
                    #[cfg(feature = "python")]
                    Some("py") => router = load_py_into_router(router, &path),
                    _ => {}
                }
            } else {
                panic!("Failed to get the file type of {entry:?}");
            }
        }

        router
    }

    #[cfg(not(feature = "python"))]
    {
        let _path = path;
        router
    }
}

pub fn setup_logger(log_file_path: &str, log_level: &str) {
    let log_level = if log_level.is_empty() {
        log::LevelFilter::Info
    } else {
        log_level.parse().expect("Log Level should be valid")
    };

    let mut dispatch = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                humantime::format_rfc3339_seconds(SystemTime::now()),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(log_level)
        .chain(std::io::stdout());

    if !log_file_path.is_empty() {
        dispatch =
            dispatch.chain(fern::log_file(log_file_path).expect("Log File should be writable"))
    }

    dispatch
        .apply()
        .expect("Logger should have initialized successfully");
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
    #[serde(default)]
    log_file_path: String,
    #[serde(default)]
    log_level: String,
}

impl HyperDomeConfig {
    pub fn from_toml_file(path: &Path) -> Self {
        let txt = read_to_string(path).expect(&format!("{path:?} should be readable"));
        toml::from_str(&txt).expect(&format!("{path:?} should be valid toml"))
    }
}

#[inline]
pub async fn async_run_router<P, I>(server: Builder<I>, mut router: Router, config: HyperDomeConfig)
where
    P: ExecutableArgs,
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

    server
        .serve(router.into_make_service())
        .with_graceful_shutdown(listen_for_commands::<P>())
        .await
        .unwrap();
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Run {
        #[arg(short, long)]
        detached: bool,
    },
}

pub fn auto_main<P: ExecutableArgs>(router: impl Fn() -> Router) {
    let Ok(args) = Args::try_parse_from(std::env::args_os()) else {
        send_args_to_remote();
        return;
    };

    match args.command {
        Commands::Run { detached } => {
            if let Some(id) = does_remote_exist() {
                println!("Remote already exists with process id: {id}");
                return;
            }
            if detached {
                let id = std::process::Command::new(
                    std::env::current_exe().expect("Current EXE name should be accessible"),
                )
                .arg("run")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("Child process should have spawned successfully")
                .id();
                println!("Process has spawned successfully with id: {id}");
                return;
            }
        }
    }

    auto_main_inner::<P>(router());
}

#[tokio::main]
async fn auto_main_inner<P: ExecutableArgs>(router: Router) {
    let config = HyperDomeConfig::from_toml_file("hypermangle.toml".as_ref());
    setup_logger(&config.log_file_path, &config.log_level);

    #[cfg(feature = "python")]
    std::thread::spawn(|| {
        pyo3::Python::with_gil(|py| {
            // Disable Ctrl-C handling
            let signal_module = py.import("signal").unwrap();
            signal_module
                .call_method1(
                    "signal",
                    (
                        signal_module.getattr("SIGINT").unwrap(),
                        signal_module.getattr("SIG_DFL").unwrap(),
                    ),
                )
                .unwrap();

            let event_loop = py
                .import("asyncio")
                .unwrap()
                .call_method0("new_event_loop")
                .unwrap();
            PY_TASK_LOCALS
                .set(pyo3_asyncio::TaskLocals::new(event_loop))
                .unwrap();
            event_loop.call_method0("run_forever").unwrap();
        })
    });

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

            info!("HTTP Certificates successfully loaded");
            async_run_router::<P, _>(
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

            let mut bind_address = config.bind_address;
            bind_address.set_port(80);
            let solver = Http01Solver::new();
            let handle = unwrap!(solver.start(&bind_address));

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

            write(cert_path, certificate.fullchain_to_pem().unwrap())
                .expect("Cert file should be writable");
            write(key_path, certificate.private_key_to_pem().unwrap())
                .expect("Key file should be writable");

            info!("Certificates successfully downloaded");

            let bind_address = config.bind_address.clone();

            async_run_router::<P, _>(
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

    async_run_router::<P, _>(axum::Server::bind(&config.bind_address), router, config).await;
}
