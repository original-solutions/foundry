//! The factory's routes: the worker routes carried between machines, and the Factory view's.

use axum::Router;

use super::Api;

/// The factory's routes, merged into [`super::router`]. None yet.
pub fn router() -> Router<Api> {
    Router::new()
}
