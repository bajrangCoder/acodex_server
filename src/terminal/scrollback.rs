use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;

pub struct Scrollback {
    path: PathBuf,
    file: Mutex<Option<File>>,
}

impl Scrollback {
    pub fn new(pid: u32) -> Self {
        let path = std::env::temp_dir().join(format!("axs_scrollback_{pid}.bin"));
        Self {
            path,
            file: Mutex::new(None),
        }
    }

    fn ensure_file(&self) -> io::Result<()> {
        let mut guard = self.file.lock().unwrap();
        if guard.is_none() {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .read(true)
                .truncate(false)
                .open(&self.path)?;
            *guard = Some(f);
        }
        Ok(())
    }

    pub fn append(&self, data: &[u8]) -> io::Result<()> {
        self.ensure_file()?;
        let mut guard = self.file.lock().unwrap();
        if let Some(ref mut f) = *guard {
            f.write_all(data)?;
        }
        Ok(())
    }

    pub fn read_tail_and_then<T, F>(&self, max_bytes: usize, then: F) -> io::Result<(Vec<u8>, T)>
    where
        F: FnOnce() -> T,
    {
        self.ensure_file()?;
        let mut guard = self.file.lock().unwrap();
        if let Some(ref mut f) = *guard {
            let file_size = f.seek(SeekFrom::End(0))?;
            let buf = if file_size == 0 {
                Vec::new()
            } else {
                let read_from = file_size.saturating_sub(max_bytes as u64);
                f.seek(SeekFrom::Start(read_from))?;
                let mut buf = Vec::with_capacity((file_size - read_from) as usize);
                f.read_to_end(&mut buf)?;
                buf
            };

            // WebSocket replay must activate live forwarding before releasing the scrollback
            // lock. Otherwise the PTY reader can append the same early output to scrollback,
            // replay it, and then forward it live again — duplicating early output.
            let result = then();
            Ok((buf, result))
        } else {
            Ok((Vec::new(), then()))
        }
    }

    pub fn cleanup(&self) {
        let mut guard = self.file.lock().unwrap();
        *guard = None;
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for Scrollback {
    fn drop(&mut self) {
        self.cleanup();
    }
}
