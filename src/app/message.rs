pub enum WorkerRequest {
    Tree(crate::ui::tree::message::TreeRequest),
    Grid(crate::ui::grid::message::GridRequest),
}

pub enum WorkerResponse {
    Tree(crate::ui::tree::message::TreeResponse),
    Grid(crate::ui::grid::message::GridResponse),
}
