//! Terminal console functionalities.

use crate::{
    attach::SharedContainerAttach,
    container_io::{ContainerIO, Message, Pipe},
    container_log::SharedContainerLog,
};
use anyhow::{bail, format_err, Context, Result};
use getset::{Getters, MutGetters};
use log::{debug, error, trace};
use nix::sys::termios::{self, OutputFlags, SetArg};
use sendfd::RecvWithFd;
use std::{
    io::ErrorKind,
    os::unix::{fs::PermissionsExt, io::RawFd},
    path::{Path, PathBuf},
    str,
    sync::mpsc::{Receiver, Sender},
    time::Duration,
};
use tempfile::Builder;
use tokio::{
    fs,
    io::{AsyncWriteExt, Interest},
    net::UnixStream,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task,
};

#[derive(Debug, Getters, MutGetters)]
pub struct Terminal {
    #[getset(get = "pub")]
    path: PathBuf,

    connected_rx: Receiver<()>,

    #[getset(get = "pub", get_mut = "pub")]
    message_rx: UnboundedReceiver<Message>,
}

#[derive(Debug, Getters)]
struct Config {
    #[get]
    path: PathBuf,

    #[get]
    ready_tx: Sender<()>,

    #[get]
    connected_tx: Sender<()>,

    #[get]
    message_tx: UnboundedSender<Message>,
}

impl Terminal {
    /// Setup a new terminal instance.
    pub fn new(logger: SharedContainerLog, attach: SharedContainerAttach) -> Result<Self> {
        debug!("Creating new terminal");
        let path = Self::temp_file_name(None, "conmon-term-", ".sock")?;
        let path_clone = path.clone();

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (connected_tx, connected_rx) = std::sync::mpsc::channel();
        let (message_tx, message_rx) = mpsc::unbounded_channel();

        task::spawn(async move {
            Self::listen(
                Config {
                    path: path_clone,
                    ready_tx,
                    connected_tx,
                    message_tx,
                },
                logger,
                attach,
            )
            .await
        });
        ready_rx.recv().context("wait for listener to be ready")?;

        Ok(Self {
            path,
            connected_rx,
            message_rx,
        })
    }

    /// Waits for the socket client to be connected.
    pub fn wait_connected(&self) -> Result<()> {
        debug!("Waiting for terminal socket connection");
        self.connected_rx
            .recv_timeout(Duration::from_secs(60))
            .context("receive connected channel")
    }

    /// Generate a the temp file name without creating the file.
    pub fn temp_file_name(directory: Option<&Path>, prefix: &str, suffix: &str) -> Result<PathBuf> {
        let mut file = Builder::new();
        file.prefix(prefix).suffix(suffix).rand_bytes(7);
        let file = match directory {
            Some(d) => file.tempfile_in(d),
            None => file.tempfile(),
        }
        .context("create tempfile")?;

        let path: PathBuf = file.path().into();
        drop(file);
        Ok(path)
    }

    async fn listen(
        config: Config,
        logger: SharedContainerLog,
        attach: SharedContainerAttach,
    ) -> Result<()> {
        let path = config.path();
        debug!("Listening terminal socket on {}", path.display());
        let listener = crate::listener::bind_long_path(path)?;

        // Update the permissions
        let mut perms = fs::metadata(path).await?.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(path, perms).await?;

        config
            .ready_tx()
            .send(())
            .map_err(|_| format_err!("unable to send ready message"))?;

        let stream = listener.accept().await?.0;
        debug!("Got terminal socket stream: {:?}", stream);

        Self::handle_fd_receive(stream, config, logger, attach).await
    }

    async fn handle_fd_receive(
        mut stream: UnixStream,
        config: Config,
        logger: SharedContainerLog,
        attach: SharedContainerAttach,
    ) -> Result<()> {
        loop {
            if !stream.ready(Interest::READABLE).await?.is_readable() {
                continue;
            }

            let mut data_buffer = [];
            let mut fd_buffer: [RawFd; 1] = [0];

            match stream.recv_with_fd(&mut data_buffer, &mut fd_buffer) {
                Ok((_, fd_read)) => {
                    // Allow only one single read
                    let path = config.path();
                    debug!("Removing socket path {}", path.display());
                    fs::remove_file(path).await?;

                    debug!("Shutting down receiver stream");
                    stream.shutdown().await?;

                    if fd_read == 0 {
                        error!("No file descriptor received");
                        bail!("got no file descriptor");
                    }

                    debug!("Received terminal file descriptor");
                    let fd = fd_buffer[0];

                    debug!("Changing terminal settings");
                    let mut term = termios::tcgetattr(fd)?;
                    term.output_flags |= OutputFlags::ONLCR;
                    termios::tcsetattr(fd, SetArg::TCSANOW, &term)?;

                    let attach_clone = attach.clone();
                    task::spawn(async move {
                        config
                            .connected_tx()
                            .send(())
                            .context("send connected channel")?;
                        ContainerIO::read_loop(
                            fd,
                            Pipe::StdOut,
                            logger,
                            config.message_tx,
                            attach_clone,
                        )
                        .await
                    });

                    task::spawn(async move { ContainerIO::read_loop_stdin(fd, attach).await });

                    // TODO: Now that we have a fd to the tty, make sure we handle any pending
                    // data that was already buffered.
                    // See: https://github.com/containers/conmon/blob/f263cf4/src/ctrl.c#L68

                    // TODO: Now that we've set mainfd_stdout, we can register the
                    // ctrl_winsz_cb if we didn't set it here, we'd risk attempting to run
                    // ioctl on a negative fd, and fail to resize the window.
                    // See: https://github.com/containers/conmon/blob/f263cf4/src/ctrl.c#L73

                    debug!("Shutting down listener thread");
                    return Ok(());
                }
                Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                    trace!("WouldBlock error, retrying");
                    continue;
                }
                Err(e) => {
                    error!("Unable to receive data: {}", e);
                    return Err(e.into());
                }
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(self.path()) {
            trace!(
                "Unable to remove socket file path {}: {}",
                self.path().display(),
                e
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{attach::SharedContainerAttach, container_log::ContainerLog};
    use nix::pty;
    use sendfd::SendWithFd;
    use std::os::unix::io::FromRawFd;

    #[tokio::test]
    async fn new_success() -> Result<()> {
        let logger = ContainerLog::new();
        let attach = SharedContainerAttach::default();

        let sut = Terminal::new(logger, attach)?;
        assert!(sut.path().exists());

        let res = pty::openpty(None, None)?;

        let stream = UnixStream::connect(sut.path()).await?;
        loop {
            let ready = stream.ready(Interest::WRITABLE).await?;
            if ready.is_writable() {
                match stream.send_with_fd(b"test", &[res.master]) {
                    Ok(_) => break,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Err(e) => bail!(e),
                }
            }
        }

        sut.wait_connected()?;
        assert!(!sut.path().exists());

        // Write to the slave
        let mut file = unsafe { fs::File::from_raw_fd(res.slave) };
        file.write_all(b"test").await?;

        Ok(())
    }
}
