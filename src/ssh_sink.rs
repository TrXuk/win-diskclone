//! SSH output sink for streaming disk image to remote system.

use std::io::Write;
use std::path::PathBuf;

use ssh2::Session;

use crate::error::Result;
use crate::local_sink::ImageSink;

/// Creates an authenticated SSH session for user@host.
/// Tries agent first, then ~/.ssh/id_ed25519 and id_rsa, then password if provided.
pub fn create_ssh_session(user: &str, host: &str, password: Option<&str>) -> Result<Session> {
    let tcp = std::net::TcpStream::connect(format!("{}:22", host))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;

    let mut authenticated = sess.userauth_agent(user).is_ok();
    if !authenticated {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from);
        if let Some(home) = home {
            let ssh_dir = home.join(".ssh");
            for key_name in ["id_ed25519", "id_rsa"] {
                let key_path = ssh_dir.join(key_name);
                if key_path.exists()
                    && sess.userauth_pubkey_file(user, None, key_path.as_path(), None).is_ok()
                {
                    authenticated = true;
                    break;
                }
            }
        }
    }
    if !authenticated {
        if let Some(pw) = password {
            if sess.userauth_password(user, pw).is_ok() {
                authenticated = true;
            }
        }
    }
    if !authenticated {
        return Err(crate::error::DiskCloneError::Other(
            "SSH authentication failed. Try: ssh-add, ensure ~/.ssh/id_ed25519 or id_rsa exists, or use password auth (--ssh-password)"
                .to_string(),
        ));
    }
    Ok(sess)
}

/// Streams disk image to a remote system via SSH.
/// Executes `dd of=<path>` or `cat > <path>` on the remote and writes to stdin.
pub struct SshSink {
    channel: ssh2::Channel,
}

impl SshSink {
    /// Connects to the remote host and starts the receive command.
    /// `remote_path` is the destination path (e.g. "/dev/sdb" or "/backup/disk.img").
    /// For block devices, the remote typically needs to run with sudo.
    pub fn new(
        session: &Session,
        remote_path: &str,
    ) -> Result<Self> {
        let escaped_path = remote_path.replace('"', "\\\"");
        let command = format!("dd of=\"{}\" bs=512 2>/dev/null", escaped_path);

        let mut channel = session.channel_session()?;
        channel.exec(&command)?;

        Ok(Self { channel })
    }

    /// Alternative: use cat for file output (simpler, no dd needed).
    pub fn new_cat(session: &Session, remote_path: &str) -> Result<Self> {
        let escaped_path = remote_path.replace('"', "\\\"");
        let escaped_path = escaped_path.replace('$', "\\$");
        let command = format!("cat > \"{}\"", escaped_path);

        let mut channel = session.channel_session()?;
        channel.exec(&command)?;

        Ok(Self { channel })
    }
}

impl ImageSink for SshSink {
    fn write(&mut self, data: &[u8]) -> Result<usize> {
        Ok(self.channel.write_all(data).map(|()| data.len())?)
    }

    fn flush(&mut self) -> Result<()> {
        self.channel.flush()?;
        self.channel.send_eof()?;
        self.channel.wait_eof()?;
        self.channel.wait_close()?;
        Ok(())
    }
}
