//! Codec initialization is retained as complete Ogg pages for late listeners.
use crate::error::ApiError;
#[derive(Default)]
pub struct Header {
    buffer: Vec<u8>,
    pages: Vec<u8>,
    packets: usize,
    complete: bool,
}
impl Header {
    pub fn push(&mut self, bytes: &[u8], opus: bool) -> Result<Option<Vec<u8>>, ApiError> {
        if self.complete {
            return Ok(None);
        }
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() + self.pages.len() > 262_144 {
            return Err(ApiError::BadGateway(
                "Audio codec header exceeds its limit".into(),
            ));
        }
        loop {
            if self.buffer.len() < 27 {
                return Ok(None);
            }
            if &self.buffer[..4] != b"OggS" || self.buffer[4] != 0 {
                return Err(ApiError::BadGateway("Invalid Ogg encoder output".into()));
            }
            let segments = usize::from(self.buffer[26]);
            if self.buffer.len() < 27 + segments {
                return Ok(None);
            }
            let payload: usize = self.buffer[27..27 + segments]
                .iter()
                .map(|size| usize::from(*size))
                .sum();
            let page = 27 + segments + payload;
            if self.buffer.len() < page {
                return Ok(None);
            }
            self.packets += self.buffer[27..27 + segments]
                .iter()
                .filter(|size| **size < 255)
                .count();
            self.pages.extend(self.buffer.drain(..page));
            if self.packets >= if opus { 2 } else { 3 } {
                self.complete = true;
                return Ok(Some(std::mem::take(&mut self.pages)));
            }
        }
    }
}

/// A burst starts at a complete non-continuation page after initialization.
pub struct Burst {
    buffer: Vec<u8>,
    after: u32,
    started: bool,
}
impl Burst {
    pub fn new(header: &[u8]) -> Result<Self, ApiError> {
        let mut cursor = 0;
        let mut after = 0;
        while cursor < header.len() {
            let page = &header[cursor..];
            if page.len() < 27 || &page[..4] != b"OggS" {
                return Err(ApiError::BadGateway("Invalid retained codec header".into()));
            }
            let count = usize::from(page[26]);
            if page.len() < 27 + count {
                return Err(ApiError::BadGateway("Truncated codec header".into()));
            }
            let size = 27
                + count
                + page[27..27 + count]
                    .iter()
                    .map(|n| usize::from(*n))
                    .sum::<usize>();
            if page.len() < size {
                return Err(ApiError::BadGateway("Truncated codec header page".into()));
            }
            after = u32::from_le_bytes(page[18..22].try_into().unwrap());
            cursor += size;
        }
        Ok(Self {
            buffer: Vec::new(),
            after,
            started: false,
        })
    }
    pub fn push(&mut self, bytes: &[u8]) -> Result<Option<Vec<u8>>, ApiError> {
        self.buffer.extend_from_slice(bytes);
        if self.buffer.len() > 262_144 {
            return Err(ApiError::BadGateway("Ogg burst exceeds its limit".into()));
        }
        let mut result = Vec::new();
        loop {
            let Some(start) = self.buffer.windows(4).position(|window| window == b"OggS") else {
                let retain = self.buffer.len().saturating_sub(3);
                self.buffer.drain(..retain);
                break;
            };
            self.buffer.drain(..start);
            if self.buffer.len() < 27 {
                break;
            }
            let count = usize::from(self.buffer[26]);
            if self.buffer[4] != 0 {
                return Err(ApiError::BadGateway("Invalid Ogg page".into()));
            }
            if self.buffer.len() < 27 + count {
                break;
            }
            let size = 27
                + count
                + self.buffer[27..27 + count]
                    .iter()
                    .map(|n| usize::from(*n))
                    .sum::<usize>();
            if self.buffer.len() < size {
                break;
            }
            let sequence = u32::from_le_bytes(self.buffer[18..22].try_into().unwrap());
            if sequence > self.after && (self.started || self.buffer[5] & 1 == 0) {
                self.started = true;
                result.extend(self.buffer.drain(..size));
            } else {
                self.buffer.drain(..size);
            }
        }
        Ok((!result.is_empty()).then_some(result))
    }
}
