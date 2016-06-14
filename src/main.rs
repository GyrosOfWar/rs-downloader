extern crate hyper; 
extern crate scoped_threadpool;
extern crate crossbeam;
#[macro_use] extern crate quick_error;

use std::path::{Path, PathBuf};
use std::io;
use std::io::prelude::*;
use std::fs::File;
use std::sync::Arc;

use hyper::client::IntoUrl;
use hyper::{Client, Url};
use scoped_threadpool::Pool;
use crossbeam::sync::MsQueue;

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
    Start { thread_id: u32, file_id: u32 },
    Downloading { bytes_read: usize }
}

pub fn download_in_parallel<U, P>(urls: Vec<U>, paths: &[P], thread_count: u32) -> DResult<()>
    where U: IntoUrl,
          P: AsRef<Path>
{
    let workitem_queue = MsQueue::new();
    let mut i = 0;
    for (url, path) in urls.into_iter().zip(paths.into_iter()) {
        let path = path.as_ref();
        let workitem = WorkItem { path: path.to_path_buf(), url: url.into_url().unwrap(), id: i };
        workitem_queue.push(workitem);
        i += 1;
    }

    let mut pool = Pool::new(thread_count);
    let client = Arc::new(Client::new());

    // let message_queue = MsQueue::new();
    pool.scoped(|scope| {
        while let Some(item) = workitem_queue.try_pop() {
            let client = client.clone();
            // let message_queue = message_queue.clone();
            scope.execute(move || {
                let mut request = client.get(item.url).send().unwrap();
                let mut writer = File::create(item.path).unwrap();
                io::copy(&mut request.watch(|n| {
                    let msg = Message::Downloading { bytes_read: n };
                    // message_queue.push(msg);
                }), &mut writer).unwrap();
            });
        }

    });

    Ok(())
}

fn main() {
    let urls = vec!["http://google.com", "http://google.com", "http://google.com", "http://google.com"];
    let paths = &["a.html", "b.html", "c.html", "d.html"];
    download_in_parallel(urls, paths, 2).unwrap();
}
