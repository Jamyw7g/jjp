use std::{
    io::{self, SeekFrom, Write},
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::StreamExt;
use reqwest::{
    header::{HeaderMap, CONTENT_RANGE, RANGE},
    Client,
};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::{
    fs::OpenOptions,
    io::{AsyncSeekExt, AsyncWriteExt, BufWriter},
    sync::Semaphore,
};

#[derive(Debug, Default)]
pub struct JjPBuilder {
    url: String,
    filename: Option<String>,
    headers: Option<HeaderMap>,
    parallel_num: Option<usize>,
    max_retries: Option<usize>,
    chunk_len: Option<usize>,
}

impl JjPBuilder {
    const CHUNK_LEN: usize = (1 << 20) * 32;

    pub fn new(url: String) -> Self {
        Self {
            url,
            ..Default::default()
        }
    }

    pub fn set_filename(mut self, filename: String) -> Self {
        self.filename = Some(filename);
        self
    }

    pub fn set_headers(mut self, headers: HeaderMap) -> Self {
        self.headers = Some(headers);
        self
    }

    pub fn set_parallel_num(mut self, parallel_num: usize) -> Self {
        self.parallel_num = Some(parallel_num);
        self
    }

    pub fn set_max_retries(mut self, max_retries: usize) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    pub fn set_chunk_len(mut self, chunk_len: usize) -> Self {
        self.chunk_len = Some(chunk_len);
        self
    }

    pub fn build(self) -> reqwest::Result<JjP> {
        let client = Client::builder()
            .http2_keep_alive_timeout(Duration::from_secs(15))
            .build()?;
        let url = self.url;
        let filename = self
            .filename
            .unwrap_or_else(|| url.split('/').last().unwrap_or("tmp.bin").to_string());
        let parallel_num = self.parallel_num.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .ok()
                .unwrap_or(unsafe { NonZeroUsize::new_unchecked(8) })
                .get()
        });
        let max_retries = self.max_retries.unwrap_or(3);
        let headers = self.headers.unwrap_or_default();
        let chunk_len = self.chunk_len.unwrap_or(Self::CHUNK_LEN);

        Ok(JjP {
            client,
            url,
            filename,
            headers,
            parallel_num,
            max_retries,
            chunk_len,
        })
    }
}

pub struct JjP {
    client: Client,
    url: String,
    filename: String,
    headers: HeaderMap,

    parallel_num: usize,
    max_retries: usize,
    chunk_len: usize,
}

impl JjP {
    pub async fn download(self) -> anyhow::Result<()> {
        let length = self.fetch_length().await;
        if tokio::fs::try_exists(&self.filename).await? {
            tokio::fs::remove_file(&self.filename).await?;
        }

        if let Some(length) = length {
            let length = length.get();
            if self.parallel_num == 1 {
                self.single_download(length).await?;
            } else {
                self.parallel_download(length).await?;
            }
        } else {
            self.single_download(0).await?;
        }

        Ok(())
    }

    async fn fetch_length(&self) -> Option<NonZeroUsize> {
        let resp = self
            .client
            .get(&self.url)
            .header(RANGE, "bytes=0-0")
            .send()
            .await
            .ok()?;

        resp.headers()
            .get(CONTENT_RANGE)
            .and_then(|val| val.to_str().ok())?
            .split('/')
            .last()?
            .parse()
            .ok()
    }

    async fn single_download(&self, length: usize) -> anyhow::Result<()> {
        let Self {
            client,
            url,
            filename,
            headers,
            max_retries,
            ..
        } = self;

        let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(1);
        let (res_tx, res_rx) = tokio::sync::oneshot::channel();
        let p_filename = filename.clone();
        let res_abort_hanlde = tokio::spawn(async move {
            progress(p_filename, length, 1, progress_rx).await;
            let _ = res_tx.send(());
        });

        let mut chunk = download_core(
            client,
            url,
            filename,
            headers.clone(),
            0,
            length,
            &progress_tx,
        )
        .await;
        let mut retries = 0;
        while let Err(ref e) = chunk {
            println!("{}", e);
            if retries >= *max_retries {
                res_abort_hanlde.abort();
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("single download retries: {}", retries),
                )
                .into());
            }
            tokio::time::sleep(Duration::from_millis(
                std::cmp::min(retries * 100, 1000) as u64
            ))
            .await;
            retries += 1;
            chunk = download_core(
                client,
                url,
                filename,
                headers.clone(),
                0,
                length,
                &progress_tx,
            )
            .await;
        }
        drop(progress_tx);
        match res_rx.await {
            Ok(_) => {
                println!("\nDone.");
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    async fn parallel_download(&self, length: usize) -> anyhow::Result<()> {
        let filename = self.filename.clone();
        let parallel_num = std::cmp::max(1, std::cmp::min(length / self.chunk_len, self.parallel_num));

        let mut abort_handles = Vec::new();

        let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(parallel_num);
        let (res_tx, res_rx) = tokio::sync::oneshot::channel();

        abort_handles.push(
            tokio::spawn(async move {
                progress(filename, length, parallel_num, progress_rx).await;
                let _ = res_tx.send(());
            })
            .abort_handle(),
        );

        let download_semap = Arc::new(Semaphore::new(parallel_num));
        let fail_semap = Arc::new(Semaphore::new(parallel_num));

        for start in (0..length).step_by(self.chunk_len) {
            // fallback to single download
            if fail_semap.clone().try_acquire_owned().is_err() {
                for handle in abort_handles {
                    handle.abort();
                }
                return self.single_download(length).await;
            }

            let owned = match tokio::time::timeout(
                Duration::from_secs(3),
                download_semap.clone().acquire_owned(),
            )
            .await
            {
                Ok(Ok(owned)) => owned,
                _ => continue,
            };
            let end = length.min(start + self.chunk_len) - 1;
            let mut headers = self.headers.clone();
            headers.insert(RANGE, format!("bytes={}-{}", start, end).try_into()?);

            let segment_length = end - start + 1;
            let client = self.client.clone();
            let progress_tx = progress_tx.clone();
            let filename = self.filename.clone();
            let url = self.url.clone();
            let headers = headers.clone();
            let fail_semap = fail_semap.clone();
            abort_handles.push(
                tokio::spawn(async move {
                    let mut chunk = download_core(
                        &client,
                        &url,
                        &filename,
                        headers.clone(),
                        start,
                        segment_length,
                        &progress_tx,
                    )
                    .await;
                    let mut retries = 0;
                    while let Err(ref e) = chunk {
                        println!("{}", e);
                        let fail_owned = fail_semap.clone().acquire_owned().await?;
                        tokio::time::sleep(Duration::from_millis(std::cmp::min(
                            retries * 100 + 100,
                            3000,
                        )))
                        .await;
                        chunk = download_core(
                            &client,
                            &url,
                            &filename,
                            headers.clone(),
                            start,
                            segment_length,
                            &progress_tx,
                        )
                        .await;
                        retries += 1;
                        drop(fail_owned);
                    }
                    drop(owned);
                    Ok::<_, anyhow::Error>(())
                })
                .abort_handle(),
            );
        }
        drop(progress_tx);
        match res_rx.await {
            Ok(_) => {
                println!("\nDone.");
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }
}

async fn download_core(
    client: &Client,
    url: &str,
    filename: &str,
    headers: HeaderMap,
    start: usize,
    length: usize,
    progress_tx: &Sender<isize>,
) -> anyhow::Result<()> {
    let mut bytes_stream = client
        .get(url)
        .headers(headers)
        .send()
        .await?
        .bytes_stream();
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&filename)
        .await?;
    file.seek(SeekFrom::Start(start as u64)).await?;
    let mut written_len = 0;
    let mut buf_writer = BufWriter::new(file);
    while let Some(bytes) = bytes_stream.next().await {
        match bytes {
            Ok(bytes) => {
                buf_writer.write_all(&bytes).await?;
                progress_tx.send(bytes.len() as isize).await?;
                written_len += bytes.len();
            }
            Err(e) => {
                progress_tx.send(-(written_len as isize)).await?;
                return Err(e.into());
            }
        }
    }
    buf_writer.flush().await?;
    if length != 0 && length != written_len {
        progress_tx.send(-(written_len as isize)).await?;
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "download chunk fail, expected length: {length}, written length: {written_len}"
            ),
        )
        .into());
    }
    Ok(())
}

async fn progress(filename: String, size: usize, parallel_num: usize, mut rx: Receiver<isize>) {
    const PART_DURATION: f32 = 1.0;

    let mut segement_len = 0;
    let mut total_len = 0;
    let size_str = to_human(size as f32);
    let cal_fn = |total_len: isize, seg_len: isize, dur: f32| {
        if total_len <= 0 || dur < f32::EPSILON || seg_len < 0 {
            return;
        }
        let speed = seg_len as f32 / dur;
        let percent = if size == 0 {
            0.0
        } else {
            total_len as f32 / size as f32
        };
        print!(
            "\rf:{} c:{} p:{:.02}% ps:{}/{} s:{}/s ",
            filename,
            parallel_num,
            (100.0 * percent),
            to_human(total_len as f32),
            size_str,
            to_human(speed as f32)
        );
        std::io::stdout().flush().unwrap();
    };
    let mut now = Instant::now();

    let mut part_flag: f32 = 0.0;

    loop {
        part_flag += 0.1;
        part_flag = part_flag.min(PART_DURATION);
        match tokio::time::timeout(Duration::from_secs_f32(PART_DURATION), rx.recv()).await {
            Ok(Some(recv_len)) => {
                segement_len += recv_len;
                total_len += recv_len;
                let elapsed = now.elapsed().as_secs_f32();
                if elapsed < part_flag {
                    continue;
                }
                cal_fn(total_len, segement_len, elapsed);
                now = Instant::now();
                segement_len = 0;
            }
            timeout_or_ok => {
                let elapsed = now.elapsed().as_secs_f32();
                cal_fn(total_len, segement_len, elapsed);
                now = Instant::now();
                segement_len = 0;
                if timeout_or_ok.is_ok() {
                    break;
                }
            }
        }
    }
}

#[inline(always)]
fn to_human(mut speed: f32) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    const STEP: f32 = 1024.0;
    let mut index = 0;

    while speed > STEP && index + 1 < UNITS.len() {
        speed /= STEP;
        index += 1;
    }
    format!("{:.02}{}", speed, UNITS[index])
}
