use axum::Router;
use hypermangle_core::auto_main;

fn main() {
    auto_main(Router::new());
}
