use std::{
    io::SeekFrom,
    iter::repeat,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use memmap2::MmapMut;
use reqwest::{header::RANGE, Client, ClientBuilder, IntoUrl, Proxy, StatusCode, Url};
use tokio::{
    fs::{self, File},
    io::{AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt},
    sync::Mutex,
    time::Instant,
};

use crate::{utils::show_bar, MapFile};

pub struct JjP {
    client: Client,
    url: Url,
    name: String,
    parallel: bool,
    parallel_num: usize,
    mmap: bool,
    length: usize,
}

trait Writer: AsyncWrite + AsyncSeek {}

impl<T: AsyncWrite + AsyncSeek> Writer for T {}

impl JjP {
    pub async fn new<U: IntoUrl>(url: U) -> anyhow::Result<Self> {
        let jb = JjpBuilder::new(url)?.build().await?;

        Ok(jb)
    }

    pub async fn run(&mut self) -> anyhow::Result<()> {
        static RUNNING: AtomicBool = AtomicBool::new(true);
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        static RECEIVED_LEN: AtomicUsize = AtomicUsize::new(0);
        static MORE: AtomicBool = AtomicBool::new(true);
        static DONE: AtomicUsize = AtomicUsize::new(0);

        MORE.store(self.parallel, Ordering::SeqCst);

        let tasks = Arc::new(Mutex::new(self.split_task()));
        let writer: Arc<Mutex<dyn Writer + Send + Unpin + 'static>> =
            if !self.parallel || !self.mmap {
                Arc::new(Mutex::new(File::create(&self.name).await?))
            } else {
                let fp = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .open(&self.name)
                    .await?;
                fp.set_len(self.length as u64).await?;
                let map = unsafe { MmapMut::map_mut(&fp)? };
                Arc::new(Mutex::new(MapFile::new(map, self.length)))
            };

        let prev_time = Instant::now();
        let mut max_len = 0;
        while RUNNING.load(Ordering::Relaxed) && DONE.load(Ordering::Relaxed) != self.parallel_num {
            let mut idx = usize::MAX;
            let mut start = 0;
            let mut end = 0;
            for (i, task) in tasks.lock().await.iter_mut().enumerate() {
                if task.done {
                    continue;
                }
                if !task.running {
                    idx = i;
                    task.running = true;
                    start = task.start_pos;
                    end = task.end_pos;
                    break;
                }
            }
            if idx != usize::MAX
                && !(!MORE.load(Ordering::Relaxed) && COUNT.load(Ordering::Relaxed) == 1)
            {
                let writer = Arc::clone(&writer);
                let client = self.client.clone();
                let url = self.url.clone();
                let parallel = self.parallel;
                let tasks = Arc::clone(&tasks);

                COUNT.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    struct Defer;
                    impl Drop for Defer {
                        fn drop(&mut self) {
                            COUNT.fetch_sub(1, Ordering::SeqCst);
                        }
                    }
                    let _g = Defer;

                    let mut rb = client.get(url);
                    if parallel {
                        rb = rb.header(RANGE, format!("bytes={}-{}", start, end));
                    }
                    let mut resp = match rb.send().await {
                        Ok(resp) => {
                            if resp.status().as_u16() / 100 != 2 {
                                let mut tasks_lck = tasks.lock().await;
                                let task = &mut tasks_lck[idx];
                                task.running = false;
                                return;
                            }
                            resp
                        }
                        Err(e) => {
                            if let Some(StatusCode::FORBIDDEN) = e.status() {
                                MORE.store(false, Ordering::SeqCst);
                            }
                            let mut tasks_lck = tasks.lock().await;
                            let task = &mut tasks_lck[idx];
                            task.running = false;
                            return;
                        }
                    };
                    loop {
                        match resp.chunk().await {
                            Ok(Some(bytes)) => {
                                let mut fp_lck = writer.lock().await;
                                if parallel {
                                    if fp_lck.seek(SeekFrom::Start(start as u64)).await.is_err() {
                                        break;
                                    }
                                }
                                if fp_lck.write_all(&bytes).await.is_err() {
                                    break;
                                }
                                start += bytes.len();
                                RECEIVED_LEN.fetch_add(bytes.len(), Ordering::SeqCst);
                            }
                            _ => break,
                        }
                    }
                    let mut task_lck = tasks.lock().await;
                    let task = &mut task_lck[idx];
                    task.start_pos = start;
                    task.running = false;
                    if start > end {
                        task.done = true;
                        DONE.fetch_add(1, Ordering::SeqCst);
                    }
                });
            }

            let received = RECEIVED_LEN.load(Ordering::Relaxed);
            let con = self.parallel_num - DONE.load(Ordering::Relaxed);
            let bar = show_bar(
                &self.name,
                received,
                self.length,
                received,
                prev_time.elapsed(),
                con,
            );
            max_len = max_len.max(bar.len());
            print!(
                "\r{}{}",
                bar,
                repeat(' ').take(max_len - bar.len()).collect::<String>()
            );
            tokio::time::sleep(Duration::from_millis(16)).await;
        }
        let received = RECEIVED_LEN.load(Ordering::Relaxed);
        let con = self.parallel_num - DONE.load(Ordering::Relaxed);
        let bar = show_bar(
            &self.name,
            received,
            self.length,
            received,
            prev_time.elapsed(),
            con,
        );
        max_len = max_len.max(bar.len());
        println!(
            "\r{}{}",
            bar,
            repeat(' ').take(max_len - bar.len()).collect::<String>()
        );

        Ok(())
    }

    fn split_task(&self) -> Vec<Task> {
        if self.parallel {
            let mut size = self.length / self.parallel_num;
            if self.length % self.parallel_num != 0 {
                size += 1;
            }
            let mut tasks = Vec::with_capacity(self.parallel_num);
            let mut next = 0;
            for _ in 0..self.parallel_num - 1 {
                let start_pos = next;
                next += size;
                let end_pos = next - 1;
                tasks.push(Task {
                    start_pos,
                    end_pos,
                    ..Default::default()
                });
            }
            tasks.push(Task {
                start_pos: next,
                end_pos: self.length - 1,
                ..Default::default()
            });
            tasks
        } else {
            vec![Task::default()]
        }
    }
}

pub struct JjpBuilder {
    url: Url,
    parallel_num: usize,
    mmap: bool,
    no_proxy: bool,
    proxy: Option<Proxy>,
    name: Option<String>,
}

impl JjpBuilder {
    pub fn new<U: IntoUrl>(url: U) -> anyhow::Result<Self> {
        Ok(Self {
            url: url.into_url()?,
            parallel_num: 4,
            mmap: true,
            no_proxy: false,
            proxy: None,
            name: None,
        })
    }

    pub fn parallel_num(mut self, num: usize) -> Self {
        if num != 0 {
            self.parallel_num = num;
        }
        self
    }

    pub fn name<N: Into<String>>(mut self, name: N) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn proxy(mut self, proxy: Proxy) -> Self {
        self.proxy = Some(proxy);
        self
    }

    pub fn no_proxy(mut self, no: bool) -> Self {
        self.no_proxy = no;
        self
    }

    pub fn mmap(mut self, mmap: bool) -> Self {
        self.mmap = mmap;
        self
    }

    pub async fn build(mut self) -> anyhow::Result<JjP> {
        let mut cb = ClientBuilder::new().user_agent("Hello");
        if let Some(proxy) = self.proxy.take() {
            cb = cb.proxy(proxy);
        }
        if self.no_proxy {
            cb = cb.no_proxy();
        }
        let client = cb.build()?;

        let (parallel, length) = crate::utils::prepare(client.clone(), self.url.clone()).await?;
        if !parallel {
            self.parallel_num = 1;
        }

        let name = if let Some(name) = self.name.take() {
            name
        } else {
            self.url
                .path()
                .rsplit('/')
                .next()
                .unwrap_or("jjp.bin")
                .to_string()
        };

        Ok(JjP {
            client,
            url: self.url,
            name,
            parallel,
            parallel_num: self.parallel_num,
            mmap: self.mmap,
            length,
        })
    }
}

struct Task {
    running: bool,
    done: bool,
    start_pos: usize,
    end_pos: usize,
}

impl Default for Task {
    fn default() -> Self {
        Self {
            running: false,
            done: false,
            start_pos: 0,
            end_pos: 0,
        }
    }
}
