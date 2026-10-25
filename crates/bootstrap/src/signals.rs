//! The signals that ask a binary to stop: Ctrl-C, and SIGTERM on Unix or
//! Ctrl-Break and a console close on Windows.

/// Listens for stop signals. See the module docs.
pub struct Signals {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(windows)]
    ctrl_break: tokio::signal::windows::CtrlBreak,
    #[cfg(windows)]
    close: tokio::signal::windows::CtrlClose,
}

impl Signals {
    pub fn listen() -> std::io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            terminate: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?,
            #[cfg(windows)]
            ctrl_break: tokio::signal::windows::ctrl_break()?,
            #[cfg(windows)]
            close: tokio::signal::windows::ctrl_close()?,
        })
    }

    /// Waits for the next signal. If listening fails, that counts as one,
    /// so the process still stops.
    pub async fn next(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = self.terminate.recv() => {}
        }
        #[cfg(windows)]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = self.ctrl_break.recv() => {}
            _ = self.close.recv() => {}
        }
    }
}
