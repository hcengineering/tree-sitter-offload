pub mod query;

#[derive(Debug, Clone, Copy)]
pub struct HighlightToken {
    pub kind_id: u16,
    pub capture_id: u16,
    pub length: u32,
}
