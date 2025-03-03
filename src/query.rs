use tree_sitter::{Node, Range, TextProvider};

pub struct RecodingUtf16TextProvider<'a> {
    text: &'a [u16],
}

impl<'a> RecodingUtf16TextProvider<'a> {
    pub fn new(text: &'a [u16]) -> Self {
        Self { text }
    }
}

pub struct RecodingUtf16TextProviderIterator<'a> {
    text: &'a [u16],
    start_offset: usize,
    end_offset: usize,
    ended: bool,
}

impl Iterator for RecodingUtf16TextProviderIterator<'_> {
    type Item = Vec<u8>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ended {
            return None;
        }
        // Expect mostly ascii
        let mut buf = Vec::with_capacity(self.end_offset - self.start_offset);
        let mut char_buf = [0u8; 4];
        for c in char::decode_utf16(
            self.text[self.start_offset..self.end_offset]
                .iter()
                .copied(),
        ) {
            let c = c.unwrap_or(char::REPLACEMENT_CHARACTER);
            let c_len = c.len_utf8();
            c.encode_utf8(&mut char_buf);
            buf.extend_from_slice(&char_buf[0..c_len]);
        }
        self.ended = true;
        Some(buf)
    }
}

impl<'a> TextProvider<Vec<u8>> for &RecodingUtf16TextProvider<'a> {
    type I = RecodingUtf16TextProviderIterator<'a>;

    fn text(&mut self, node: Node) -> Self::I {
        let start_offset = node.start_byte() / 2;
        let end_offset = node.end_byte() / 2;

        RecodingUtf16TextProviderIterator {
            text: self.text,
            start_offset,
            end_offset,
            ended: false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct CaptureOffset {
    start_offset: i32,
    end_offset: i32,
}

impl CaptureOffset {
    pub fn new(start_offset: i32, end_offset: i32) -> Self {
        Self {
            start_offset,
            end_offset,
        }
    }

    pub fn apply_to_range(&self, range: &Range) -> Range {
        let start_byte = ((range.start_byte as i32) + self.start_offset) as usize;
        let end_byte = ((range.end_byte as i32) + self.start_offset) as usize;
        let start_point = range.start_point;
        let start_point = tree_sitter::Point {
            row: start_point.row,
            column: ((start_point.column as i32) + self.start_offset) as usize,
        };
        let end_point = range.end_point;
        let end_point = tree_sitter::Point {
            row: end_point.row,
            column: ((end_point.column as i32) + self.end_offset) as usize,
        };
        Range {
            start_byte,
            end_byte,
            start_point,
            end_point,
        }
    }
}
