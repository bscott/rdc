//! Follow a local audit file: seed from its tail, then poll for appended lines. Handles the
//! file being rotated (size shrinks) or not existing yet.

use super::Log;
use crate::server::audit::{Entry, tail};
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(500);
const SEED: usize = 200;

pub async fn follow(path: PathBuf, log: Arc<Log>) {
    let mut pos: u64 = 0;
    let mut seeded = false;
    let mut partial = String::new();
    loop {
        let len = match tokio::fs::metadata(&path).await {
            Ok(m) => m.len(),
            Err(_) => {
                tokio::time::sleep(POLL).await;
                continue;
            }
        };
        if !seeded {
            if let Ok(entries) = tail(&path, SEED) {
                for e in entries {
                    log.push(e);
                }
            }
            pos = len;
            seeded = true;
        }
        if len < pos {
            // Rotated or truncated: start over from the top of the new file.
            pos = 0;
            partial.clear();
        }
        if len > pos {
            let p = path.clone();
            let read = tokio::task::spawn_blocking(move || -> std::io::Result<String> {
                let mut f = std::fs::File::open(&p)?;
                f.seek(SeekFrom::Start(pos))?;
                let mut buf = String::new();
                f.take(len - pos).read_to_string(&mut buf)?;
                Ok(buf)
            })
            .await;
            if let Ok(Ok(chunk)) = read {
                pos = len;
                partial.push_str(&chunk);
                while let Some(nl) = partial.find('\n') {
                    let line: String = partial.drain(..=nl).collect();
                    if let Ok(e) = serde_json::from_str::<Entry>(line.trim()) {
                        log.push(e);
                    }
                }
            }
        }
        tokio::time::sleep(POLL).await;
    }
}
