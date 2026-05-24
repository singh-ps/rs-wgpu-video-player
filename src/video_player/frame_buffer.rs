use std::sync::Arc;

#[derive(Debug)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Arc<[u8]>,
    pub ts_us: u64,
}
