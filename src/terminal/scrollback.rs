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

    pub fn read_tail(&self, max_bytes: usize) -> io::Result<Vec<u8>> {
        self.ensure_file()?;
        let mut guard = self.file.lock().unwrap();
        if let Some(ref mut f) = *guard {
            let file_size = f.seek(SeekFrom::End(0))?;
            if file_size == 0 {
                return Ok(Vec::new());
            }

            let read_from = file_size.saturating_sub(max_bytes as u64);

            f.seek(SeekFrom::Start(read_from))?;
            let mut buf = Vec::with_capacity((file_size - read_from) as usize);
            f.read_to_end(&mut buf)?;
            Ok(buf)
        } else {
            Ok(Vec::new())
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
