pub enum WorkerRequest {
    Tree(crate::ui::tree::message::TreeRequest),
    Grid(crate::ui::grid::message::GridRequest),
    Query(crate::app::run::QueryRequest),
}

pub enum WorkerResponse {
    Tree(crate::ui::tree::message::TreeResponse),
    Grid(crate::ui::grid::message::GridResponse),
    Query(crate::app::run::QueryResponse),
}

/// Tags a `WorkerResponse` with the session it belongs to. The tag is
/// unforgeable by construction: a worker task is spawned bound to exactly
/// one `SessionId` (see `app::worker::spawn`/`spawn_canceller`) and can only
/// ever stamp its own id onto the responses it sends.
pub struct SessionResponse {
    pub session: crate::app::session::SessionId,
    pub response: WorkerResponse,
}
