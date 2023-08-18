use hypermangle_core::{auto_main, hypermangle_main, pyo3, pyo3_asyncio, PyResult};

#[hypermangle_main(flavor = "multi_thread")]
async fn main() -> PyResult<()> {
    auto_main(|x| x).await;
    Ok(())
}
