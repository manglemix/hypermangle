use hypermangle_core::{auto_main, PyResult, hypermangle_main, pyo3_asyncio, pyo3};

#[hypermangle_main(flavor = "multi_thread")]
async fn main() -> PyResult<()> {
    auto_main(|x| x).await;
    Ok(())
}
