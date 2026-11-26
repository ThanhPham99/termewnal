//! Thread-safe terminal session output logger (plain ANSI-stripped or raw bytes).

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use alacritty_terminal::vte::{Parser as VteParser, Perform};

/// Writes terminal session output to disk, optionally stripping ANSI/VT escape sequences.
pub struct SessionLogger {
    path: PathBuf,
    strip_ansi: bool,
    writer: BufWriter<File>,
    parser: VteParser,
}

impl SessionLogger {
    /// Opens or creates a log file at `path`, appending any new output.
    pub fn start(path: impl Into<PathBuf>, strip_ansi: bool) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            path,
            strip_ansi,
            writer: BufWriter::new(file),
            parser: VteParser::new(),
        })
    }

    /// Returns the filesystem path of the active log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends terminal stream bytes to the log file.
    pub fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if !self.strip_ansi {
            self.writer.write_all(bytes)?;
            return self.writer.flush();
        }

        let mut sink = StripSink {
            out: Vec::with_capacity(bytes.len()),
        };
        self.parser.advance(&mut sink, bytes);
        if !sink.out.is_empty() {
            self.writer.write_all(&sink.out)?;
            self.writer.flush()?;
        }
        Ok(())
    }

    /// Flushes any buffered output to disk.
    pub fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Builds a sanitized default log file path inside `logs_dir` for `session_title`.
pub fn default_session_log_path(logs_dir: &Path, session_title: &str) -> PathBuf {
    let clean: String = session_title
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let base = if clean.is_empty() {
        "session".to_string()
    } else {
        clean
    };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    logs_dir.join(format!("{base}-{ts}.log"))
}

struct StripSink {
    out: Vec<u8>,
}

impl Perform for StripSink {
    fn print(&mut self, c: char) {
        let mut buf = [0_u8; 4];
        let s = c.encode_utf8(&mut buf);
        self.out.extend_from_slice(s.as_bytes());
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' | b'\t' => self.out.push(byte),
            _ => {}
        }
    }
}
