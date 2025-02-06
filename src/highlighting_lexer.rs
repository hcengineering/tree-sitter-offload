use crate::LanguageId;

pub mod query;

#[derive(Debug, Clone, Copy)]
pub struct HighlightToken {
    pub language_id: LanguageId,
    pub kind_id: u16,
    pub capture_id: u16,
    pub length: u32,
}
