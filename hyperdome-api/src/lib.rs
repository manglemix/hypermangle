use axum::{body::HttpBody, Router};

pub struct HyperDomeAPI<S, B> {
    router: Router<S, B>,
}

impl<S, B> HyperDomeAPI<S, B> {
    pub fn borrow_router<S2, B2>(
        self,
        f: impl FnOnce(Router<S, B>) -> Router<S2, B2>,
    ) -> HyperDomeAPI<S2, B2> {
        HyperDomeAPI {
            router: f(self.router),
        }
    }

    pub fn destructure(self) -> (Router<S, B>,) {
        (self.router,)
    }
}

impl<S, B> HyperDomeAPI<S, B>
where
    B: HttpBody + Send + 'static,
    S: Clone + Send + Sync + 'static,
{
    pub fn new() -> Self {
        Self {
            router: Router::new(),
        }
    }
}
