use std::{io::SeekFrom, sync::Arc, time::Duration};

use jjp::MapFile;
use tokio::{
    fs,
    io::{AsyncSeekExt, AsyncWriteExt},
    sync::Mutex,
};

fn main() -> anyhow::Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let fp = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open("test.txt")
            .await?;
        let size: usize = 1024 * 1024 * 1024;
        let chunk_size = size / 8;

        fp.set_len(size as u64).await?;
        let map = unsafe { memmap2::MmapMut::map_mut(&fp)? };
        let map_file = Arc::new(Mutex::new(MapFile::new(map, size)));

        let futs = (0..8)
            .map(|i| {
                let map_file = Arc::clone(&map_file);

                tokio::spawn(async move {
                    let buf: Vec<_> = (0..4096).map(|num| (num % 256) as u8).collect();
                    for idx in (0..chunk_size).step_by(4096) {
                        let mut file_lck = map_file.lock().await;
                        file_lck
                            .seek(SeekFrom::Start((i * chunk_size + idx) as u64))
                            .await?;
                        file_lck.write_all(&buf).await?;
                        tokio::time::sleep(Duration::from_nanos(152)).await;
                    }
                    Ok::<_, anyhow::Error>(())
                })
            })
            .collect::<Vec<_>>();
        for f in futs.into_iter() {
            _ = f.await?;
        }

        Ok(())
    })
}
