//! Expect/send `LoginScript` automation runner shared across SSH, Telnet, and Serial sessions.

use std::time::Duration;

use alacritty_terminal::vte::{Parser as VteParser, Perform};
use regex::Regex;
use termewnal_core::{LoginScriptStep, vault::SecretStore};
use zeroize::Zeroizing;

/// Action emitted by [`LoginScriptRunner`] when an expect pattern matches (or an immediate step runs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoginScriptDispatch {
    pub delay: Duration,
    pub bytes: Zeroizing<Vec<u8>>,
}

enum ExpectMatcher {
    Immediate,
    Literal(String),
    Regex(Regex),
}

struct CompiledStep {
    matcher: ExpectMatcher,
    payload: Zeroizing<Vec<u8>>,
    delay: Duration,
}

/// Stateful expect/send runner scanning incoming ANSI-stripped stream bytes.
pub struct LoginScriptRunner {
    steps: Vec<CompiledStep>,
    cursor: usize,
    parser: VteParser,
    window: String,
}

impl LoginScriptRunner {
    /// Compiles `steps`, resolving any `secret_ref` payloads from `vault` without storing plaintext.
    pub fn compile(steps: &[LoginScriptStep], vault: Option<&dyn SecretStore>) -> Self {
        let mut compiled = Vec::with_capacity(steps.len());
        for step in steps {
            let matcher = if step.expect.is_empty() {
                ExpectMatcher::Immediate
            } else if step.is_regex {
                match Regex::new(&step.expect) {
                    Ok(re) => ExpectMatcher::Regex(re),
                    Err(_) => ExpectMatcher::Literal(step.expect.clone()),
                }
            } else {
                ExpectMatcher::Literal(step.expect.clone())
            };

            let secret_text = step
                .secret_ref
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .and_then(|r| vault.and_then(|v| v.get_secret(r).ok().flatten()));

            let mut raw = match secret_text {
                Some(sec) => sec.as_bytes().to_vec(),
                None => step.send.as_bytes().to_vec(),
            };
            if step.append_newline && !raw.ends_with(b"\r") && !raw.ends_with(b"\n") {
                raw.push(b'\r');
            }

            compiled.push(CompiledStep {
                matcher,
                payload: Zeroizing::new(raw),
                delay: Duration::from_millis(step.delay_ms),
            });
        }

        Self {
            steps: compiled,
            cursor: 0,
            parser: VteParser::new(),
            window: String::new(),
        }
    }

    /// Returns `true` when all steps have been dispatched.
    pub fn is_finished(&self) -> bool {
        self.cursor >= self.steps.len()
    }

    /// Resets execution back to step 0 (e.g., on transport reconnect).
    pub fn reset(&mut self) {
        self.cursor = 0;
        self.window.clear();
        self.parser = VteParser::new();
    }

    /// Feeds incoming stream `bytes` and returns any ready script replies in execution order.
    pub fn advance(&mut self, bytes: &[u8]) -> Vec<LoginScriptDispatch> {
        if self.is_finished() {
            return Vec::new();
        }
        if !bytes.is_empty() {
            let mut collector = PlainTextCollector {
                out: &mut self.window,
            };
            self.parser.advance(&mut collector, bytes);
            const MAX_WINDOW: usize = 8192;
            if self.window.len() > MAX_WINDOW {
                let mut cut = self.window.len() - MAX_WINDOW;
                while !self.window.is_char_boundary(cut) && cut < self.window.len() {
                    cut += 1;
                }
                self.window.drain(..cut);
            }
        }

        let mut out = Vec::new();
        while let Some(step) = self.steps.get(self.cursor) {
            let match_end = match &step.matcher {
                ExpectMatcher::Immediate => Some(0),
                ExpectMatcher::Literal(lit) => {
                    self.window.find(lit.as_str()).map(|idx| idx + lit.len())
                }
                ExpectMatcher::Regex(re) => re.find(&self.window).map(|m| m.end()),
            };
            let Some(end) = match_end else {
                break;
            };
            out.push(LoginScriptDispatch {
                delay: step.delay,
                bytes: step.payload.clone(),
            });
            if end > 0 && end <= self.window.len() {
                self.window.drain(..end);
            }
            self.cursor += 1;
        }
        out
    }
}

struct PlainTextCollector<'a> {
    out: &'a mut String,
}

impl Perform for PlainTextCollector<'_> {
    fn print(&mut self, c: char) {
        self.out.push(c);
    }

    fn execute(&mut self, byte: u8) {
        if matches!(byte, b'\n' | b'\r' | b'\t') {
            self.out.push(byte as char);
        }
    }
}
