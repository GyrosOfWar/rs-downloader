#![cfg_attr(feature="clippy", feature(plugin))]
#![cfg_attr(feature="clippy", plugin(clippy))]
#![allow(enum_variant_names)]

extern crate hyper;
extern crate scoped_threadpool;
extern crate crossbeam;
#[macro_use]
extern crate quick_error;
extern crate thread_id;
extern crate rustbox;
extern crate number_prefix;
extern crate clap;

mod util;

use std::path::{Path, PathBuf};
use std::{io, thread};
use std::io::prelude::*;
use std::fs::File;
use std::sync::Arc;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use hyper::client::IntoUrl;
use hyper::{Client, Url};
use hyper::header::ContentLength;
use scoped_threadpool::Pool;
use crossbeam::sync::MsQueue;
use rustbox::{RustBox, Color, Key};
use number_prefix::{decimal_prefix, Standalone, Prefixed};
use clap::{App, Arg};

use util::DurationExt;

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
    StartFile {
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

fn fmt_bytes(bytes: f32) -> String {
    match decimal_prefix(bytes) {
        Standalone(bytes) => format!("{} bytes", bytes),
        Prefixed(prefix, n) => format!("{:.0} {}B", n, prefix),
    }
}

#[derive(Debug)]
struct Progress {
    file_name: String,
    file_size: Option<u64>,
    progress: u64,
    error: Option<DError>,
    start_time: Instant,
    download_rate: f64,
}

impl Progress {
    pub fn new() -> Progress {
        Progress {
            file_name: String::new(),
            file_size: None,
            progress: 0,
            error: None,
            start_time: Instant::now(),
            download_rate: 0.0
        }
    }
    
    fn fmt_file_size(&self) -> String {
        match self.file_size {
            Some(sz) => fmt_bytes(sz as f32),
            None => String::from("?"),
        }
    }

    fn fmt_progress_percent(&self) -> String {
        match self.file_size {
            Some(sz) => format!("{:.2}%", (self.progress as f64 / sz as f64) * 100.0),
            None => String::new(),
        }
    }

    fn fmt_progress_bytes(&self) -> String {
        fmt_bytes(self.progress as f32)
    }

    fn fmt_download_rate(&self) -> String {
        format!("{}/s", fmt_bytes(self.download_rate as f32))
    }
}

struct DownloadWatcher {
    status_map: HashMap<usize, Progress>,
    rustbox: RustBox,
    quitting: bool,
    num_files: usize,
    files_finished: usize,
}

impl DownloadWatcher {
    pub fn new(num_files: usize) -> DownloadWatcher {
        DownloadWatcher {
            status_map: HashMap::new(),
            rustbox: RustBox::init(Default::default()).unwrap(),
            quitting: false,
            num_files: num_files,
            files_finished: 0,
        }
    }

    pub fn process(&mut self, message: Message) -> bool {
        if self.quitting {
            return true;
        }

        match message {
            Message::Done => return true,
            Message::StartFile { thread_id, file_name, file_size } => {
                self.status_map.insert(thread_id,
                                       Progress {
                                           file_name: file_name,
                                           file_size: file_size,
                                           progress: 0,
                                           error: None,
                                           start_time: Instant::now(),
                                           download_rate: 0.0,
                                       });
            }
            Message::Success { thread_id } => {
                self.status_map.remove(&thread_id);
                self.files_finished += 1;
            }
            Message::Downloading { thread_id, bytes_read } => {                
                let mut e = self.status_map.get_mut(&thread_id).unwrap();
                let new_progress = e.progress + bytes_read;
                let elapsed = e.start_time.elapsed();
                let download_rate = new_progress as f64 / elapsed.seconds();
                e.download_rate = download_rate;
                e.progress = new_progress;
            }
            Message::Error { err, thread_id } => {
                let e = self.status_map.entry(thread_id).or_insert_with(Progress::new);
                e.error = Some(err);
            }
        }
        false
    }

    #[allow(single_match)]
    pub fn output(&mut self) {
        self.rustbox.clear();
        self.rustbox.print(0,
                           0,
                           rustbox::RB_NORMAL,
                           Color::White,
                           Color::Black,
                           &format!("Files downloaded: {}/{}",
                                    self.files_finished,
                                    self.num_files));
        let width = self.rustbox.width();
        let m = width / 2;
        for (y, progress) in self.status_map.values().enumerate() {
            let y = y + 1;
            let name = if progress.file_name.len() >= m {
                &progress.file_name[..m]
            } else {
                &progress.file_name
            };
            
            
            let p = format!("{}/{}",
                            progress.fmt_progress_bytes(),
                            progress.fmt_file_size());
            self.rustbox.print(0, y, rustbox::RB_NORMAL, Color::White, Color::Black, name);
            self.rustbox.print(m,
                               y,
                               rustbox::RB_NORMAL,
                               Color::White,
                               Color::Black,
                               &progress.fmt_progress_percent());
            self.rustbox.print(m + 10, y, rustbox::RB_NORMAL, Color::White, Color::Black, &p);
            self.rustbox.print(width - 15, y, rustbox::RB_NORMAL, Color::White, Color::Black, &progress.fmt_download_rate());
        }

        self.rustbox.present();

        match self.rustbox.peek_event(Duration::from_millis(16), false) {
            Ok(rustbox::Event::KeyEvent(key)) => {
                match key {
                    Key::Char('q') => self.quitting = true,
                    _ => {}
                }
            }
            Err(e) => panic!("{}", e),
            _ => {}
        }
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

pub fn download_in_parallel<U, P>(urls: Vec<U>, paths: &[P], thread_count: u32, timeout: u64, quiet: bool) -> DResult<()>
    where U: IntoUrl,
          P: AsRef<Path>
{
    if urls.len() != paths.len() {
        panic!("Not enough paths for URLs")
    }

    let file_count = urls.len();
    let workitem_queue = MsQueue::new();
    for (i, (url, path)) in urls.into_iter().zip(paths.into_iter()).enumerate() {
        let path = path.as_ref();
        let workitem = WorkItem {
            path: path.to_path_buf(),
            url: url.into_url().unwrap(),
            id: i as u32,
        };
        workitem_queue.push(workitem);
    }

    let mut pool = Pool::new(thread_count);
    // let client = Arc::new(Client::new());

    let message_queue = Arc::new(MsQueue::new());
    pool.scoped(|scope| {
        while let Some(item) = workitem_queue.try_pop() {
            let message_queue = message_queue.clone();
            scope.execute(move || {
                let mut client = Client::new();
                client.set_read_timeout(Some(Duration::from_secs(timeout)));
                let request = try_or_send!(client.get(item.url).send(), message_queue);
                let length = request.headers.get::<ContentLength>().map(|c| c.0);
                let path = item.path;
                let mut writer = try_or_send!(File::create(path.clone()), message_queue);
                let file_name: String = path.file_name().unwrap().to_str().unwrap().into();

                message_queue.push(Message::StartFile {
                    thread_id: thread_id::get(),
                    file_name: file_name,
                    file_size: length,
                });
                try_or_send!(io::copy(&mut request.watch(|n| {
                                          message_queue.push(Message::Downloading {
                                              bytes_read: n as u64,
                                              thread_id: thread_id::get(),
                                          })
                                      }),
                                      &mut writer),
                             message_queue);

                message_queue.push(Message::Success { thread_id: thread_id::get() });
            });
        }
        let message_queue = message_queue.clone();
        // Progress watcher thread
        if !quiet {
            thread::spawn(move || {
                let mut download_watcher = DownloadWatcher::new(file_count);
                let dt = Duration::from_millis(25);
                let mut last = Instant::now();
                loop {
                    let msg = message_queue.pop();
                    if download_watcher.process(msg) {
                        break;
                    }
                    if last.elapsed() > dt {
                        download_watcher.output();
                        last = Instant::now();
                    }
                }
            });
        }
    });
    message_queue.push(Message::Done);

    Ok(())
}

fn read_urls(path: &str) -> (Vec<Url>, Vec<String>) {
    let mut urls = vec![];
    let mut paths = vec![];

    let reader = io::BufReader::new(File::open(path).unwrap());

    for line in reader.lines() {
        let url = Url::parse(line.unwrap().trim()).unwrap();
        let k = url.path().rfind('/').unwrap();
        let name = &url.path()[k + 1..];
        paths.push(format!("downloads/{}", name));
        urls.push(url.clone());
    }

    (urls, paths)
}

fn main() {
    let matches = App::new("downloader")
        .version("0.1.0")
        .author("Martin Tomasi <martin.tomasi@gmail.com>")
        .about("Downloads multiple files in parallel.")
        .arg(Arg::with_name("file")
            .short("f")
            .long("file")
            .help("Text file with an URL on each line")
            .takes_value(true)
            .required(true))
        .arg(Arg::with_name("threads")
            .short("t")
            .long("threads")
            .help("Sets the number of concurrent downloads")
            .takes_value(true))
        .arg(Arg::with_name("timeout")
             .long("timeout")
             .help("HTTP timeout per thread in seconds.")
             .takes_value(true))
        .arg(Arg::with_name("quiet")
             .short("q")
             .long("quiet")
             .help("No UI, no output."))
        .get_matches();

    let filepath = matches.value_of("file").unwrap();
    let (urls, paths) = read_urls(filepath);
    let thread_count = matches.value_of("threads").map_or(4, |s| s.parse::<u32>().unwrap());
    let timeout = matches.value_of("timeout").map_or(15, |s| s.parse::<u64>().unwrap());
    let quiet = matches.is_present("quiet");
    let start = Instant::now();
    download_in_parallel(urls, &paths, thread_count, timeout, quiet).unwrap();
    let elapsed = start.elapsed();
    println!("#Threads: {}, Duration: {} seconds", thread_count, elapsed.seconds());
}
