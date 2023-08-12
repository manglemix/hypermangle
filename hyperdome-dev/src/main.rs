use std::fs::{create_dir_all, read_to_string};
use std::path::Path;
use std::process::{Command, Stdio};

use axum::Router;
use hyperdome_core::{async_run_router, load_scripts_into_router, setup_logger, HyperDomeConfig};

use clap::{self, Parser, Subcommand};
use log::warn;
use pyo3::PyResult;
use pyo3_asyncio::tokio::re_exports::runtime::Handle;
use serde::Deserialize;

const CACHE_NAME: &str = ".hyperdome";
const GIT_REPO_URL: &str = "https://github.com/manglemix/hyperdome.git";
const BOILERPLATE_RS: &str = include_str!("boilerplate.rs");

/// HTTP Server scripted with python
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Build { preset: Option<String> },
    Run,
}

fn make_cache() {
    let path = Path::new(CACHE_NAME);
    if path.exists() {
        assert!(path.is_dir(), "{CACHE_NAME} exists but is not a directory");
    } else {
        std::fs::create_dir(path).expect(&format!(
            "Creation of {CACHE_NAME} folder should be successful"
        ));
    }
}

fn create_cache_project_toml(path: &Path, version: &str, project_name: &str) {
    let toml = format!(
        "
[package]
name = \"{project_name}\"
version = \"{version}\"
edition = \"2021\"

[dependencies]
hyperdome-core = {{ \"path\" = \"../hyperdome/hyperdome-core\"]}}
hyperdome-api = {{ \"path\" = \"../hyperdome/hyperdome-api\" }}
{project_name} = {{ \"path\" = \"../../{}\" }}
",
        path.to_str().unwrap()
    );

    std::fs::write(path.join("Cargo.toml"), toml).expect("Cargo.toml should be writable");
}

#[derive(Deserialize)]
struct PackageInfo {
    name: String,
    version: String,
}

#[derive(Deserialize)]
struct CargoConfig {
    package: PackageInfo,
}

fn static_link_project(path: &Path, _preset: String) {
    make_cache();
    let cargo_config =
        read_to_string(path.join("Cargo.toml")).expect("Cargo.toml should be readable");
    let cargo_config: CargoConfig = toml::from_str(&cargo_config)
        .expect("Cargo.toml should be a valid cargo configuration file");

    let cache_proj_path = Path::new(CACHE_NAME).join(&cargo_config.package.name);

    let mut git_cmd = Command::new("git");
    if Path::new(CACHE_NAME).join("hyperdome").exists() {
        git_cmd
            .arg("pull")
            .current_dir(Path::new(CACHE_NAME).join("hyperdome"));
    } else {
        git_cmd
            .args(["clone", "--depth", "1", GIT_REPO_URL])
            .current_dir(CACHE_NAME);
    }
    git_cmd
        .stderr(Stdio::inherit())
        .stdout(Stdio::inherit())
        .output()
        .expect("git should be installed and accessible");

    if cache_proj_path.exists() {
    } else {
        let src_path = cache_proj_path.join(Path::new("src"));
        create_dir_all(&src_path).expect(&format!("{src_path:?} should be creatable"));
        std::fs::write(src_path.join("main.rs"), BOILERPLATE_RS)
            .expect("Creation or main.rs should be successful");

        create_cache_project_toml(
            &cache_proj_path,
            &cargo_config.package.version,
            &cargo_config.package.name,
        );
    }

    Command::new("cargo")
        .arg("build")
        .current_dir(cache_proj_path)
        .stderr(Stdio::inherit())
        .stdout(Stdio::inherit())
        .output()
        .expect("cargo should be installed and accessible");
}

fn try_build_rust_project(preset: String) -> bool {
    for entry in Path::new(".")
        .read_dir()
        .expect("Current directory should be readable")
    {
        let entry = entry.expect("Items in the current directory should be readable");
        let file_type = entry
            .file_type()
            .expect(&format!("File type of {entry:?} should be accessible"));
        if !file_type.is_dir() {
            continue;
        }

        let path = entry.path();
        if path.join("Cargo.toml").exists() {
            static_link_project(&path, preset);
            return true;
        }
    }
    false
}

#[pyo3_asyncio::tokio::main(flavor = "multi_thread")]
async fn main() -> PyResult<()> {
    let args = Args::parse();
    setup_logger();

    match args.command {
        Commands::Build { preset } => {
            if !try_build_rust_project(preset.unwrap_or_default()) {
                warn!("No rust project found!");
            }
        }
        Commands::Run => {
            let built = try_build_rust_project(String::new());

            if built {
                todo!()
            } else if !Path::new("scripts").exists() {
                warn!("There is no scripts folder nor rust project to run!");
            } else {
                let config = HyperDomeConfig::from_toml_file("hyperdome.toml".as_ref());
                async_run_router(
                    load_scripts_into_router(Router::new(), "scripts".as_ref(), Handle::current()),
                    config,
                )
                .await;
            }
        }
    };

    Ok(())
}
