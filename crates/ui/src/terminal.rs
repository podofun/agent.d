//! Terminal panes: a program's output parsed by a terminal emulator and sent
//! to clients as a grid of styled runs, never as raw bytes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::{Run, Screen};

pub type TerminalResize = Box<dyn Fn(u16, u16) + Send + Sync>;

/// A running program's terminal: what it prints, where its input goes, how
/// to resize it, and whatever must live as long as it (dropping `keep`
/// stops the program).
pub struct TerminalIo {
    pub reader: Box<dyn AsyncRead + Send + Unpin>,
    pub writer: Box<dyn AsyncWrite + Send + Unpin>,
    pub resize: TerminalResize,
    pub keep: Box<dyn Send>,
}

/// A live terminal pane. Dropping it stops the program.
pub(crate) struct Terminal {
    input: mpsc::UnboundedSender<Vec<u8>>,
    parser: Arc<Mutex<vt100::Parser>>,
    resize: TerminalResize,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    _keep: Box<dyn Send>,
}

impl Drop for Terminal {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Terminal {
    /// Start pumping `io`. `screen` is called with each new screen, at most
    /// about 30 times a second.
    pub(crate) fn start(
        io: TerminalIo,
        width: u16,
        height: u16,
        screen: impl Fn(Screen) + Send + Sync + 'static,
    ) -> Self {
        let (width, height) = (width.max(2), height.max(1));
        (io.resize)(width, height);
        let parser = Arc::new(Mutex::new(vt100::Parser::new(height, width, 0)));
        let dirty = Arc::new(tokio::sync::Notify::new());
        let (input, mut inputs) = mpsc::unbounded_channel::<Vec<u8>>();
        let mut reader = io.reader;
        let mut writer = io.writer;
        let read = {
            let parser = parser.clone();
            let dirty = dirty.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            parser.lock().unwrap().process(&buf[..n]);
                            dirty.notify_one();
                        }
                    }
                }
                dirty.notify_one();
            })
        };
        let write = tokio::spawn(async move {
            while let Some(bytes) = inputs.recv().await {
                if writer.write_all(&bytes).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
        });
        let draw = {
            let parser = parser.clone();
            tokio::spawn(async move {
                loop {
                    dirty.notified().await;
                    let grid = grid(&parser.lock().unwrap());
                    screen(grid);
                    tokio::time::sleep(Duration::from_millis(33)).await;
                }
            })
        };
        Self {
            input,
            parser,
            resize: io.resize,
            tasks: vec![read, write, draw],
            _keep: io.keep,
        }
    }

    pub(crate) fn input(&self, data: &str) {
        let _ = self.input.send(data.as_bytes().to_vec());
    }

    pub(crate) fn resize(&self, width: u16, height: u16) {
        let (width, height) = (width.max(2), height.max(1));
        let mut parser = self.parser.lock().unwrap();
        if parser.screen().size() != (height, width) {
            parser.set_size(height, width);
            (self.resize)(width, height);
        }
    }
}

fn color(c: vt100::Color) -> Option<String> {
    match c {
        vt100::Color::Default => None,
        vt100::Color::Idx(i) => Some(format!("@{i}")),
        vt100::Color::Rgb(r, g, b) => Some(format!("#{r:02x}{g:02x}{b:02x}")),
    }
}

/// The emulator's screen as rows of styled runs.
pub(crate) fn grid(parser: &vt100::Parser) -> Screen {
    let screen = parser.screen();
    let (height, width) = screen.size();
    let mut lines = Vec::with_capacity(height as usize);
    for row in 0..height {
        let mut runs: Vec<Run> = Vec::new();
        for col in 0..width {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let mut text = crate::sanitize(&cell.contents());
            if text.is_empty() || text == "\n" {
                text = " ".into();
            }
            let style = Run {
                text: String::new(),
                fg: color(cell.fgcolor()),
                bg: color(cell.bgcolor()),
                bold: cell.bold(),
                italic: cell.italic(),
                underline: cell.underline(),
                inverse: cell.inverse(),
            };
            match runs.last_mut() {
                Some(last)
                    if Run {
                        text: String::new(),
                        ..last.clone()
                    } == style =>
                {
                    last.text.push_str(&text)
                }
                _ => runs.push(Run { text, ..style }),
            }
        }
        lines.push(runs);
    }
    let cursor = (!screen.hide_cursor()).then(|| {
        let (row, col) = screen.cursor_position();
        (col, row)
    });
    Screen {
        width,
        height,
        lines,
        cursor,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_become_styles_and_never_reach_the_grid() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"\x1b[31mred\x1b[0m ok\x1b]0;title\x07");
        let screen = grid(&parser);
        assert_eq!(screen.lines[0][0].text, "red");
        assert_eq!(screen.lines[0][0].fg.as_deref(), Some("@1"));
        let all: String = screen
            .lines
            .iter()
            .flatten()
            .map(|r| r.text.as_str())
            .collect();
        assert!(!all.chars().any(char::is_control));
    }
}
