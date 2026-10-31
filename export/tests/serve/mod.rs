//! A small HTTP/1.1 server for a folder's files, answering GETs with or without a range, for the
//! tests and the benchmark. It counts the requests it answers and the bytes they carry, and can wait
//! before each answer, as a server far away would.

use std::{
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, UNIX_EPOCH},
};

pub struct Server {
    /// `http://127.0.0.1:PORT`: a file's URL is this, `/` and its name.
    pub base: String,
    pub requests: Arc<AtomicU64>,
    pub bytes: Arc<AtomicU64>,
    /// Whether it answers range requests, or sends whole files instead.
    pub ranges: Arc<AtomicBool>,
}

impl Server {
    /// Serves `dir`'s files, waiting `latency` before each answer.
    pub fn start(dir: PathBuf, latency: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let server = Self {
            base: format!("http://{}", listener.local_addr().unwrap()),
            requests: Arc::default(),
            bytes: Arc::default(),
            ranges: Arc::new(AtomicBool::new(true)),
        };
        let (requests, bytes, ranges) = (
            server.requests.clone(),
            server.bytes.clone(),
            server.ranges.clone(),
        );
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (dir, requests, bytes, ranges) =
                    (dir.clone(), requests.clone(), bytes.clone(), ranges.clone());
                std::thread::spawn(move || {
                    let _ = serve(stream, &dir, latency, &requests, &bytes, &ranges);
                });
            }
        });
        server
    }

    pub fn url(&self, name: &str) -> String {
        format!("{}/{name}", self.base)
    }

    /// Requests and bytes answered so far.
    pub fn counts(&self) -> (u64, u64) {
        (
            self.requests.load(Ordering::Relaxed),
            self.bytes.load(Ordering::Relaxed),
        )
    }
}

fn serve(
    stream: TcpStream,
    dir: &std::path::Path,
    latency: Duration,
    requests: &AtomicU64,
    bytes: &AtomicU64,
    ranges: &AtomicBool,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut stream = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let path = line.split(' ').nth(1).unwrap_or("/").to_owned();
        let mut range = None;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header)?;
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':')
                && name.eq_ignore_ascii_case("range")
            {
                range = value.trim().strip_prefix("bytes=").map(str::to_owned);
            }
        }
        std::thread::sleep(latency);
        requests.fetch_add(1, Ordering::Relaxed);

        let name = path.split('?').next().unwrap().trim_start_matches('/');
        let Ok(mut file) = std::fs::File::open(dir.join(name)) else {
            stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")?;
            continue;
        };
        let meta = file.metadata()?;
        let size = meta.len();
        // Changes with the file
        let modified = meta
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let etag = format!("\"{size:x}-{modified:x}\"");
        let span = range
            .filter(|_| ranges.load(Ordering::Relaxed))
            .and_then(|r| {
                let (from, to) = r.split_once('-')?;
                let from: u64 = from.parse().ok()?;
                // Nothing to give of an empty file: 416
                let Some(end) = size.checked_sub(1) else {
                    return Some((1, 0));
                };
                let to = to.parse().map_or(end, |to: u64| to.min(end));
                Some((from, to))
            });
        let (status, from, to) = match span {
            Some((from, to)) if from <= to => ("206 Partial Content", from, to),
            Some(_) => {
                stream.write_all(
                    format!("HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{size}\r\nContent-Length: 0\r\n\r\n").as_bytes(),
                )?;
                continue;
            }
            None => ("200 OK", 0, size.saturating_sub(1)),
        };
        let length = if size == 0 { 0 } else { to - from + 1 };
        let mut body = vec![0; length as usize];
        file.seek(SeekFrom::Start(from))?;
        file.read_exact(&mut body)?;
        let mut head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {length}\r\nETag: {etag}\r\nAccept-Ranges: bytes\r\n"
        );
        if span.is_some() {
            head += &format!("Content-Range: bytes {from}-{to}/{size}\r\n");
        }
        head += "\r\n";
        // Counted before it can be read, so a query done reading is counted in full
        bytes.fetch_add(length, Ordering::Relaxed);
        stream.write_all(head.as_bytes())?;
        stream.write_all(&body)?;
    }
}
