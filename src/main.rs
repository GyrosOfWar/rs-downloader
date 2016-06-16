extern crate hyper;
extern crate scoped_threadpool;
extern crate crossbeam;
#[macro_use]
extern crate quick_error;
extern crate thread_id;
extern crate rustbox;

use std::path::{Path, PathBuf};
use std::{io, env};
use std::io::prelude::*;
use std::fs::File;
use std::sync::Arc;
use std::time::Duration;
use std::collections::HashMap;

use hyper::client::IntoUrl;
use hyper::{Client, Url};
use hyper::header::ContentLength;
use scoped_threadpool::Pool;
use crossbeam::sync::MsQueue;
use rustbox::{RustBox, Color};

macro_rules! printfl {
    ($($tt:tt)*) => {{
        print!($($tt)*);
        ::std::io::stdout().flush().ok().expect("flush() fail");
    }}
}

pub struct Watcher<R, F> {
    pub inner: R,
    pub f: F,
}

impl<R, F> Read for Watcher<R, F>
    where R: Read,
          F: FnMut(usize)
{
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let result = self.inner.read(buf);
        if let Ok(n) = result {
            if n > 0 {
                (self.f)(n);
            }
        }
        result
    }
}

trait WatchRead {
    fn watch<F>(self, f: F) -> Watcher<Self, F>
        where Self: Sized
    {
        Watcher {
            inner: self,
            f: f,
        }
    }
}

impl<R> WatchRead for R where R: Read {}
quick_error! {
    #[derive(Debug)]
    pub enum DError {
        IoError(err: io::Error) {
            from()
            description("IO error")
            display("IO error: {}", err)
            cause(err)
        }
        HyperError(err: hyper::Error) {
            from()
            description("HTTP error")
            display("HTTP error: {}", err)
            cause(err)
        }
    }
}

pub type DResult<T> = Result<T, DError>;

#[derive(Debug)]
struct WorkItem {
    path: PathBuf,
    url: Url,
    id: u32,
}

#[derive(Debug)]
enum Message {
    Start {
        thread_id: usize,
        file_name: String,
        file_size: Option<u64>,
    },
    Downloading {
        bytes_read: u64,
        thread_id: usize,
    },
    Success {
        thread_id: usize,
    },
    Error {
        thread_id: usize,
        err: DError,
    },
    Done,
}

#[derive(Debug)]
struct Progress {
    file_name: String,
    file_size: Option<u64>,
    progress: u64,
    error: Option<DError>,
}

impl Progress {
    pub fn new() -> Progress {
        Progress {
            file_name: String::new(),
            file_size: None,
            progress: 0,
            error: None
        }
    }
}

struct DownloadWatcher {
    status_map: HashMap<usize, Progress>,
    rustbox: RustBox
}

impl DownloadWatcher {
    pub fn new() -> DownloadWatcher {
        DownloadWatcher { 
            status_map: HashMap::new(),
            rustbox: RustBox::init(Default::default()).unwrap()
        }
    }

    pub fn process(&mut self, message: Message) -> bool {
        match message {
            Message::Done => return true,
            Message::Start { thread_id, file_name, file_size } => {
                self.status_map.insert(thread_id,
                                       Progress {
                                           file_name: file_name,
                                           file_size: file_size,
                                           progress: 0,
                                           error: None,
                                       });
            }
            Message::Success { thread_id } => {
                self.status_map.remove(&thread_id);
            }
            Message::Downloading { thread_id, bytes_read } => {
                let mut e = self.status_map.get_mut(&thread_id).unwrap();
                e.progress += bytes_read;
            },
            Message::Error { err, thread_id } => {
                let e = self.status_map.entry(thread_id).or_insert(Progress::new());
                e.error = Some(err);
            }
        }
        false
    }

    pub fn output(&self) {
        self.rustbox.clear();

        for (y, progress) in self.status_map.values().enumerate() {
            self.rustbox.print(0, y, rustbox::RB_NORMAL, Color::White, Color::Black, &format!("{:?}", progress));
        }

        self.rustbox.present();
    }
}

macro_rules! try_or_send {
    ($expr: expr, $queue: expr) => (match $expr {
        Ok(val) => val,
        Err(err) => {
            $queue.push(Message::Error { thread_id: thread_id::get(), err: From::from(err) });
            return;
        }
    })
}

pub fn download_in_parallel<U, P>(urls: Vec<U>, paths: &[P], thread_count: u32) -> DResult<()>
    where U: IntoUrl,
          P: AsRef<Path>
{
    if urls.len() != paths.len() {
        panic!("Not enough paths for URLs")
    }
    
    let workitem_queue = MsQueue::new();
    let mut i = 0;
    for (url, path) in urls.into_iter().zip(paths.into_iter()) {
        let path = path.as_ref();
        let workitem = WorkItem {
            path: path.to_path_buf(),
            url: url.into_url().unwrap(),
            id: i,
        };
        workitem_queue.push(workitem);
        i += 1;
    }

    let mut pool = Pool::new(thread_count);
    let client = Arc::new(Client::new());

    let message_queue = Arc::new(MsQueue::new());
    pool.scoped(|scope| {
        while let Some(item) = workitem_queue.try_pop() {
            let client = client.clone();
            let message_queue = message_queue.clone();
            scope.execute(move || {
                let request = try_or_send!(client.get(item.url).send(), message_queue);
                let length = request.headers.get::<ContentLength>().map(|c| c.0);
                let path = item.path;
                let mut writer = try_or_send!(File::create(path.clone()), message_queue);
                let file_name: String = path.file_name().unwrap().to_str().unwrap().into();
                
                message_queue.push(Message::Start {
                    thread_id: thread_id::get(),
                    file_name: file_name,
                    file_size: length,
                });
                try_or_send!(io::copy(
                    &mut request.watch(|n| {
                        message_queue.push(Message::Downloading {
                            bytes_read: n as u64,
                            thread_id: thread_id::get(),
                        })
                    }),
                    &mut writer
                ), message_queue);

                message_queue.push(Message::Success { thread_id: thread_id::get() });
            });
        }
        
        let mut download_watcher = DownloadWatcher::new();
        loop {    
            let msg = message_queue.pop();
            if download_watcher.process(msg) {
                break;
            }
            download_watcher.output();
        }
    });
    message_queue.push(Message::Done); 

    Ok(())
}

fn main() {
    let args: Vec<_> = env::args().collect();
    let mut urls = vec![];
    let mut paths: Vec<String> = vec![];

    if let Some(idx) = args.iter().position(|c| c == "-f") {
        let filename = args.get(idx + 1).expect("Missing argument to -f");
        let reader = io::BufReader::new(File::open(filename).unwrap());
        for line in reader.lines() {
            let url = Url::parse(line.unwrap().trim()).unwrap();
            let k = url.path().rfind('/').unwrap();
            let name = &url.path()[k+1..];
            paths.push(format!("downloads/{}", name));
            urls.push(url.clone());

        }
    }
    else {
        for url in &args[1..] {
            let url = Url::parse(url).unwrap();
            paths.push(url.path().into());
            urls.push(url);
        }
    }
    download_in_parallel(urls, &paths, 8).unwrap();
}
