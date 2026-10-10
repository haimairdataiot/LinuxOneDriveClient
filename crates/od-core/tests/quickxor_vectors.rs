//! rclone's published QuickXorHash test vectors (MIT), extracted in
//! docs/research/02-microsoft-graph-api.md §11.3.

use base64::Engine as _;
use od_core::quickxor::{QuickXorHash, hash_bytes};

#[derive(serde::Deserialize)]
struct Vector {
    size: usize,
    #[serde(rename = "in")]
    input: String,
    out: String,
}

#[test]
fn all_rclone_vectors_match() {
    let vectors: Vec<Vector> = serde_json::from_str(include_str!("quickxor_vectors.json")).unwrap();
    assert_eq!(vectors.len(), 70);
    for v in &vectors {
        let data = base64::engine::general_purpose::STANDARD.decode(&v.input).unwrap();
        assert_eq!(data.len(), v.size);
        assert_eq!(hash_bytes(&data), v.out, "one-shot, size {}", v.size);
        for block in [1usize, 2, 4, 7, 8, 16, 32, 64, 128, 256, 512] {
            let mut h = QuickXorHash::new();
            for c in data.chunks(block) {
                h.update(c);
            }
            assert_eq!(h.finalize_base64(), v.out, "block {block}, size {}", v.size);
        }
    }
}
