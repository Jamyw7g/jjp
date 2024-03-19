use std::{
    io::{self, SeekFrom, Write},
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{stream::FuturesUnordered, StreamExt};
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

        let (tx, rx) = tokio::sync::mpsc::channel(parallel_num);
        let headers = self.headers.unwrap_or_default();
        let chunk_len = self.chunk_len.unwrap_or((1 << 20) * 8);

        Ok(JjP {
            client,
            url,
            filename,
            headers,
            parallel_num,
            max_retries,
            chunk_len,
            tx,
            rx,
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

    tx: Sender<usize>,
    rx: Receiver<usize>,
}

impl JjP {
    pub async fn parallel_download(self) -> anyhow::Result<()> {
        let length = self.fetch_length().await;
        if tokio::fs::try_exists(&self.filename).await? {
            tokio::fs::remove_file(&self.filename).await?;
        }

        let mut group = FuturesUnordered::new();
        let rx = self.rx;
        let filename = self.filename.clone();
        let parallel_num = self.parallel_num;
        group.push(tokio::spawn(async move {
            let total_len = length.map(|len| len.get()).unwrap_or_default();
            progress(filename, total_len, parallel_num, rx).await;
            Ok::<_, anyhow::Error>(())
        }));

        let download_semap = Arc::new(Semaphore::new(self.parallel_num));
        if let Some(length) = length {
            let length = length.get();
            for start in (0..length).step_by(self.chunk_len) {
                let end = length.min(start + self.chunk_len) - 1;
                let mut headers = self.headers.clone();
                headers.insert(RANGE, format!("bytes={}-{}", start, end).try_into()?);

                let segment_length = end - start + 1;
                let client = self.client.clone();
                let progress_tx = self.tx.clone();
                let filename = self.filename.clone();
                let url = self.url.clone();
                let download_semap = download_semap.clone();
                let max_retries = self.max_retries;
                let headers = headers.clone();
                group.push(tokio::spawn(async move {
                    let _semap = download_semap.acquire_owned().await?;

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
                    while let Err(_) = chunk {
                        if retries >= max_retries {
                            return Err(io::Error::new(
                                io::ErrorKind::Other,
                                format!("retries {}, max_retries: {}", retries, max_retries),
                            )
                            .into());
                        }
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
                    }
                    Ok(())
                }));
            }
        } else {
            println!("unkown length");
        }

        drop(self.tx);
        while let Some(join) = group.next().await {
            match join {
                Ok(Ok(_)) => (),
                Ok(Err(e)) => println!("download error: {e:?}"),
                Err(e) => println!("group error: {e:?}"),
            }
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
}

async fn download_core(
    client: &Client,
    url: &str,
    filename: &str,
    headers: HeaderMap,
    start: usize,
    length: usize,
    progress_tx: &Sender<usize>,
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
        let bytes = bytes?;
        buf_writer.write_all(&bytes).await?;
        progress_tx.send(bytes.len()).await?;
        written_len += bytes.len();
    }
    buf_writer.flush().await?;
    if length != 0 && length != written_len {
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

async fn progress(filename: String, size: usize, parallel_num: usize, mut rx: Receiver<usize>) {
    const PART_DURATION: u64 = 1;

    let mut now = Instant::now();
    let mut segement_len = 0;
    let mut total_len = 0;
    let cal_fn = |total_len: usize, seg_len: usize, dur: u64| {
        if total_len == 0 || dur == 0 {
            return;
        }
        let speed = seg_len as u64 / dur;
        let percent = total_len as f32 / size as f32;
        print!("\rf:{} c:{} p:{:.02}% s:{} ", filename, parallel_num, (100.0 * percent), to_human(speed as f32));
        std::io::stdout().flush().unwrap();
    };
    while let Some(recv_len) = rx.recv().await {
        let elapsed = now.elapsed().as_secs();
        segement_len += recv_len;
        total_len += recv_len;
        if elapsed < PART_DURATION {
            continue;
        }
        now = Instant::now();
        cal_fn(total_len, segement_len, elapsed);
        segement_len = 0;
    }
    let elapsed = now.elapsed().as_secs();
    cal_fn(total_len, segement_len, elapsed);
    println!("Done.");
}

#[inline(always)]
fn to_human(mut speed: f32) -> String {
    const UNITS: [&str; 5] = ["bytes", "Kbytes", "Mbytes", "Gbytes", "Tbytes"];
    const STEP: f32 = 1024.0;
    let mut index = 0;

    while speed > STEP && index+1 < UNITS.len() {
        speed /= STEP;
        index += 1;
    }
    format!("{:.02} {}", speed, UNITS[index])
}
