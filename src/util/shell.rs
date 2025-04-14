use std::fmt::Display;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use crossterm::cursor::MoveToColumn;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};
use static_assertions::assert_impl_all;

pub struct Shell {
    inner: Mutex<ShellImpl>,
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

assert_impl_all!(Shell: Send, Sync);

impl Shell {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ShellImpl::new()),
        }
    }

    /// This prints a line that is "transient" in the sense that anything that is
    /// printed after it will overwrite it. This will break if s contains any newlines,
    /// or really any other terminal control characters.
    /// 
    /// To strip out new line characters, consider using [WithoutNewlines].
    pub fn print_transient(&self, s: impl Display) -> io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.clear_if_needed();

        write!(inner.stderr, "{}", s)?;
        inner.stderr.flush()?;
        inner.set_needs_clearing(true);

        Ok(())
    }

    pub fn err(&self) -> StderrWriter<'_> {
        let mut inner = self.inner.lock().unwrap();
        inner.clear_if_needed();
        StderrWriter { inner }
    }

    pub fn out(&self) -> StdoutWriter<'_> {
        let mut inner = self.inner.lock().unwrap();
        inner.clear_if_needed();
        StdoutWriter { inner }
    }

    pub fn owned_err(self: Arc<Shell>) -> OwnedStderrWriter {
        OwnedStderrWriter { inner: self }
    }
}

struct ShellImpl {
    stdout: Box<dyn Write + Send>,
    stderr: Box<dyn Write + Send>,
    line_needs_clearing: bool,
}

impl ShellImpl {
    pub fn new() -> Self {
        Self {
            stdout: Box::new(anstream::stdout()),
            stderr: Box::new(anstream::stderr()),
            line_needs_clearing: false,
        }
    }

    pub fn set_needs_clearing(&mut self, b: bool) {
        self.line_needs_clearing = b;
    }

    pub fn clear_current_line_err(&mut self) -> io::Result<()> {
        execute!(self.stderr, MoveToColumn(0), Clear(ClearType::UntilNewLine))?;
        self.set_needs_clearing(false);

        Ok(())
    }

    fn clear_if_needed(&mut self) {
        if self.line_needs_clearing {
            // This is how Cargo does it (i.e. ignoring the error)
            let _ = self.clear_current_line_err();
        }
    }
}

impl Drop for ShellImpl {
    fn drop(&mut self) {
        self.clear_if_needed();
    }
}

pub struct OwnedStderrWriter {
    inner: Arc<Shell>,
}

impl Write for OwnedStderrWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.err().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.err().flush()
    }
}

pub struct StderrWriter<'a> {
    inner: MutexGuard<'a, ShellImpl>,
}

impl Write for StderrWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.stderr.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.stderr.flush()
    }
}

pub struct StdoutWriter<'a> {
    inner: MutexGuard<'a, ShellImpl>,
}

impl Write for StdoutWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.stdout.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.stdout.flush()
    }
}
