//! Used for /dev/zero - infinitely returns zero
//! which is useful for commands like `dd if=/dev/zero of=bigfile.img size=1G`

use std::io::{self, *};

use crate::FileDescriptor;
use crate::VirtualFile;

#[derive(Debug, Default)]
pub struct ZeroFile {}

impl Seek for ZeroFile {
    fn seek(&mut self, _pos: SeekFrom) -> io::Result<u64> {
        Ok(0)
    }
}
impl Write for ZeroFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Read for ZeroFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        buf.fill(0);
        Ok(buf.len())
    }
}

impl VirtualFile for ZeroFile {
    fn last_accessed(&self) -> u64 {
        0
    }
    fn last_modified(&self) -> u64 {
        0
    }
    fn created_time(&self) -> u64 {
        0
    }
    fn size(&self) -> u64 {
        0
    }
    fn set_len(&mut self, _new_size: u64) -> crate::Result<()> {
        Ok(())
    }
    fn unlink(&mut self) -> crate::Result<()> {
        Ok(())
    }
    fn bytes_available(&self) -> crate::Result<usize> {
        Ok(0)
    }
    fn get_fd(&self) -> Option<FileDescriptor> {
        None
    }
}
