use std::{
    io::{self, ErrorKind, SeekFrom},
    pin::Pin,
    task::{Context, Poll},
};

use memmap2::MmapMut;
use tokio::io::{AsyncSeek, AsyncWrite};

pub struct MapFile {
    map: MmapMut,
    len: usize,
    write_pos: usize,
    flush_pos: usize,
}

impl MapFile {
    pub fn new(map: MmapMut, len: usize) -> Self {
        Self {
            map,
            len,
            write_pos: 0,
            flush_pos: 0,
        }
    }
}

impl AsyncWrite for MapFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let remainder_len = self.len - self.write_pos;
        let amt = remainder_len.min(buf.len());
        unsafe {
            self.map
                .as_mut_ptr()
                .add(self.write_pos)
                .copy_from(buf.as_ptr(), amt);
        }
        self.write_pos += amt;
        Poll::Ready(Ok(amt))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let flush_len = self.write_pos - self.flush_pos;
        self.map.flush_async_range(self.flush_pos, flush_len)?;
        self.flush_pos += flush_len;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for MapFile {
    fn start_seek(mut self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        match position {
            SeekFrom::Current(pos) => {
                let pos_a = pos.unsigned_abs();
                let offset_res = if pos < 0 {
                    self.write_pos.checked_sub(pos_a as usize)
                } else {
                    self.write_pos.checked_add(pos_a as usize)
                };
                self.write_pos = offset_res
                    .ok_or_else(|| io::Error::new(ErrorKind::PermissionDenied, "Out of range"))?;
            }
            SeekFrom::Start(pos) => {
                if pos > self.len as u64 {
                    return Err(io::Error::new(ErrorKind::PermissionDenied, "Out of range"));
                }
                self.write_pos = pos as usize;
            }
            SeekFrom::End(pos) => {
                let pos_a = pos.unsigned_abs();
                if pos > 0 || pos_a > self.len as u64 {
                    return Err(io::Error::new(ErrorKind::PermissionDenied, "Out of range"));
                }
                self.write_pos = self.len - pos_a as usize;
            }
        }
        self.flush_pos = self.write_pos;
        Ok(())
    }

    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        Poll::Ready(Ok(self.write_pos as u64))
    }
}

#[cfg(test)]
mod tests {
    use core::slice;

    use super::*;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    #[tokio::test]
    async fn map_write() -> anyhow::Result<()> {
        let mut map = MmapMut::map_anon(32).unwrap();
        let ptr = map.as_mut_ptr();
        let len = map.len();
        let mut map_file = MapFile::new(map, len);

        let data = [1, 2, 3, 4, 5, 6];
        map_file.write_all(&data).await?;

        let buf = unsafe { slice::from_raw_parts(ptr, 6) };
        assert_eq!(buf, &data);

        drop(map_file);
        Ok(())
    }

    #[tokio::test]
    async fn map_seek() -> anyhow::Result<()> {
        let mut map = MmapMut::map_anon(32).unwrap();
        let ptr = map.as_mut_ptr();
        let len = map.len();
        let mut map_file = MapFile::new(map, len);

        let data = [1, 2, 3, 4, 5, 6];
        map_file.write_all(&data).await?;
        map_file.seek(SeekFrom::Start(4)).await?;
        map_file.write_all(&data).await?;

        let ok = map_file.seek(SeekFrom::End(1)).await;
        assert!(ok.is_err());
        let ok = map_file.seek(SeekFrom::Current(22)).await;
        assert!(ok.is_ok());

        let buf = unsafe { slice::from_raw_parts(ptr.add(4), 6) };
        assert_eq!(buf, &data);

        drop(map_file);
        Ok(())
    }
}
