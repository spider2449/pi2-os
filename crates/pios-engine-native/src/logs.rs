//! Container output log: size-capped files with rotation, binary records
//! (`stream u8, time_ns i64 LE, len u32 LE, data`) so any output, including
//! non UTF-8, round-trips exactly. `container.log` is the newest file,
//! `container.log.1` the one before, and so on.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};

pub const STDOUT: u8 = 1;
pub const STDERR: u8 = 2;
const HEADER: usize = 13;

pub struct LogWriter {
    path: PathBuf,
    file: File,
    size: u64,
    max_size: u64,
    max_files: u32,
}

impl LogWriter {
    pub fn open(path: &Path, max_size: u64, max_files: u32) -> io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(Self { path: path.to_path_buf(), file, size, max_size: max_size.max(4096), max_files: max_files.max(1) })
    }

    pub fn write(&mut self, stream: u8, time_ns: i64, data: &[u8]) -> io::Result<()> {
        if self.size > 0 && self.size + (HEADER + data.len()) as u64 > self.max_size {
            self.rotate()?;
        }
        let mut rec = Vec::with_capacity(HEADER + data.len());
        rec.push(stream);
        rec.extend_from_slice(&time_ns.to_le_bytes());
        rec.extend_from_slice(&(data.len() as u32).to_le_bytes());
        rec.extend_from_slice(data);
        self.file.write_all(&rec)?;
        self.size += rec.len() as u64;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        for i in (1..self.max_files).rev() {
            let from = numbered(&self.path, i - 1);
            if from.exists() {
                std::fs::rename(&from, numbered(&self.path, i))?;
            }
        }
        if self.max_files == 1 {
            let _ = std::fs::remove_file(&self.path);
        }
        self.file = OpenOptions::new().create(true).write(true).truncate(true).open(&self.path)?;
        self.size = 0;
        Ok(())
    }
}

fn numbered(path: &Path, i: u32) -> PathBuf {
    if i == 0 {
        path.to_path_buf()
    } else {
        PathBuf::from(format!("{}.{i}", path.display()))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub stream: u8,
    pub time_ns: i64,
    pub data: Vec<u8>,
}

/// All records, oldest first. A torn final record (power cut) is ignored.
pub fn read_all(path: &Path, max_files: u32) -> Vec<Record> {
    let mut out = Vec::new();
    for i in (0..max_files.max(1)).rev() {
        let Ok(f) = File::open(numbered(path, i)) else { continue };
        let mut r = BufReader::new(f);
        let mut head = [0u8; HEADER];
        while r.read_exact(&mut head).is_ok() {
            let time_ns = i64::from_le_bytes(head[1..9].try_into().unwrap());
            let len = u32::from_le_bytes(head[9..13].try_into().unwrap()) as usize;
            let mut data = vec![0; len];
            if r.read_exact(&mut data).is_err() {
                break;
            }
            out.push(Record { stream: head[0], time_ns, data });
        }
    }
    out
}

pub fn remove_all(path: &Path, max_files: u32) {
    for i in 0..max_files.max(1) {
        let _ = std::fs::remove_file(numbered(path, i));
    }
}

/// "10m", "512k", "1g" or plain bytes.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let s = s.trim_end_matches('b');
    let (num, mult) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1u64 << 10),
        'm' => (&s[..s.len() - 1], 1 << 20),
        'g' => (&s[..s.len() - 1], 1 << 30),
        _ => (s, 1),
    };
    num.trim().parse::<f64>().ok().filter(|n| *n >= 0.0).map(|n| (n * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("pios-logs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d.join("container.log")
    }

    #[test]
    fn round_trips_records_across_rotation() {
        let p = tmp("rot");
        let mut w = LogWriter::open(&p, 4096, 3).unwrap();
        let chunk = vec![b'x'; 1000];
        for i in 0..10 {
            w.write(if i % 2 == 0 { STDOUT } else { STDERR }, i, &chunk).unwrap();
        }
        let recs = read_all(&p, 3);
        // Only the files still on disk: 3 x 4096 bytes hold at most 12 records.
        assert!(recs.len() >= 8 && recs.len() <= 10, "{}", recs.len());
        assert_eq!(recs.last().unwrap().time_ns, 9);
        assert!(recs.windows(2).all(|w| w[0].time_ns < w[1].time_ns));
        assert!(!numbered(&p, 3).exists());
        remove_all(&p, 3);
        assert!(read_all(&p, 3).is_empty());
    }

    #[test]
    fn ignores_torn_tail_and_reopens() {
        let p = tmp("torn");
        let mut w = LogWriter::open(&p, 1 << 20, 1).unwrap();
        w.write(STDOUT, 1, b"hello\n").unwrap();
        drop(w);
        OpenOptions::new().append(true).open(&p).unwrap().write_all(&[1, 0, 0]).unwrap();
        let recs = read_all(&p, 1);
        assert_eq!(recs, vec![Record { stream: STDOUT, time_ns: 1, data: b"hello\n".to_vec() }]);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("10m"), Some(10 << 20));
        assert_eq!(parse_size("512k"), Some(512 << 10));
        assert_eq!(parse_size("2048"), Some(2048));
        assert_eq!(parse_size("1.5g"), Some(3 << 29));
        assert_eq!(parse_size("x"), None);
    }
}
