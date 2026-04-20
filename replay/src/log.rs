use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

use flowlab_core::event::Event;

/// Append-only binary event log writer.
pub struct EventLogWriter {
    file: File,
    count: u64,
}

impl EventLogWriter {
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;

        Ok(Self { file, count: 0 })
    }

    pub fn append(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .append(true)
            .open(path)?;

        let size = file.metadata()?.len();
        let count = size / (size_of::<Event>() as u64);

        Ok(Self { file, count })
    }

    pub fn write_event(&mut self, event: &Event) -> std::io::Result<()> {
        let bytes = bytemuck::bytes_of(event);
        self.file.write_all(bytes)?;
        self.count += 1;
        Ok(())
    }

    pub fn write_batch(&mut self, events: &[Event]) -> std::io::Result<()> {
        let bytes = bytemuck::cast_slice(events);
        self.file.write_all(bytes)?;
        self.count += events.len() as u64;
        Ok(())
    }

    pub fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }

    pub fn count(&self) -> u64 {
        self.count
    }
}
