use axum::{body::Body, Router};

pub struct HyperDomeAPI<S = (), B = Body> {
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

impl HyperDomeAPI
{
    pub fn new() -> Self {
        Self {
            router: Router::new(),
        }
    }
}
