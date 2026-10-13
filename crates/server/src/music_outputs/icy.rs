//! Opt-in ICY metadata preserves audio byte boundaries across arbitrary chunks.
pub const INTERVAL: usize = 16384;
#[derive(Default)]
pub struct Interleave {
    used: usize,
}
impl Interleave {
    pub fn push(&mut self, bytes: &[u8], title: &str) -> Vec<u8> {
        let mut result = Vec::with_capacity(bytes.len() + 1600);
        for chunk in bytes.chunks(INTERVAL) {
            let boundary = (INTERVAL - self.used).min(chunk.len());
            result.extend_from_slice(&chunk[..boundary]);
            self.used += boundary;
            if self.used == INTERVAL {
                self.used = 0;
                result.extend(metadata(title));
            }
            result.extend_from_slice(&chunk[boundary..]);
            self.used += chunk.len() - boundary;
        }
        result
    }
}
fn metadata(title: &str) -> Vec<u8> {
    let safe = title
        .chars()
        .take(384)
        .map(|character| match character {
            '\'' => '’',
            ';' => ',',
            character if character.is_control() => ' ',
            character => character,
        })
        .collect::<String>();
    let mut bytes = format!("StreamTitle='{safe}';StreamUrl='';").into_bytes();
    let blocks = bytes.len().div_ceil(16);
    bytes.resize(blocks * 16, 0);
    let mut result = Vec::with_capacity(bytes.len() + 1);
    result.push(u8::try_from(blocks).expect("384 Unicode scalars fit an ICY block"));
    result.extend(bytes);
    result
}
