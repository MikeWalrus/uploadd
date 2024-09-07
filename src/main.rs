#![feature(let_chains)]
use std::{
    fs::{create_dir, File},
    io::{BufRead, BufReader, BufWriter, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::Command,
    str::FromStr,
};

use clap::Parser;
use fs_extra::{file::move_file, file::CopyOptions};
use httparse::{Header, Request};
use memchr::{arch::all::is_prefix, memmem};

#[derive(Parser)]
struct Args {
    #[arg(short, long)]
    output_dir: String,

    #[arg(short, long)]
    cmd: Option<String>,
}

fn main() {
    let options = Args::parse();

    let listener = TcpListener::bind("127.0.0.1:12141").unwrap();

    for stream in listener.incoming() {
        let stream = stream.unwrap();
        handle_connection(stream, &options);
        println!("Connection established!");
    }
}

fn find_header_value<T>(headers: &[Header], name: &str) -> T
where
    T: FromStr,
    T::Err: std::fmt::Debug,
{
    let name = name.to_owned().to_ascii_lowercase();
    let header = headers
        .iter()
        .find(|h| h.name.to_owned().to_ascii_lowercase() == name)
        .unwrap();
    std::str::from_utf8(header.value)
        .unwrap()
        .parse::<T>()
        .unwrap()
}

fn handle_connection(stream: TcpStream, options: &Args) {
    let reader = stream.try_clone().unwrap();
    let mut writer = stream;
    let mut reader = BufReader::new(reader);
    let mut buf: Vec<u8> = Vec::new();

    'outer: loop {
        let received = reader.fill_buf().unwrap();
        if received.is_empty() {
            eprintln!("received nothing");
            continue;
        }
        let received_len = received.len();
        buf.extend(received);
        reader.consume(received_len);
        dbg!(received_len);
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = Request::new(&mut headers);
        let result = req.parse(&buf).unwrap();
        if result.is_complete() {
            if req.method.unwrap() != "POST" {
                write!(writer, "HTTP/1.1 200 OK\r\n\r\nNot Supported").unwrap();
                return;
            }
            eprintln!("is complete");
            let header_len = result.unwrap();
            dbg!(header_len);
            dbg!(&headers);
            let content_len: usize = find_header_value(&headers, "Content-Length");
            let content_type: String = find_header_value(&headers, "Content-Type");
            let boundary = parse_boundary(&content_type);
            dbg!(&content_type);
            let body_received = &buf[header_len - b"\r\n".len()..];
            let body = body_received.chain(reader.into_inner());
            handle_request(body, boundary, content_len, writer, options);
            break 'outer;
        }
        eprintln!("is not complete");
    }
}

fn parse_boundary(content_type: &str) -> &str {
    let (ty, parameter) = content_type.split_once(';').unwrap();
    assert_eq!(ty, "multipart/form-data");
    let (k, v) = parameter.split_once('=').unwrap();
    assert_eq!(k.trim(), "boundary");
    v
}

struct Buf {
    inner: Box<[u8]>,
    len: usize,
}

impl Buf {
    fn with_capacity(c: usize) -> Self {
        Buf {
            inner: std::iter::repeat(0).take(c).collect(),
            len: 0,
        }
    }

    fn consume_and_read(&mut self, amt: usize, read: &mut impl Read) {
        let new_len = self.len - amt;
        if new_len > 0 {
            unsafe {
                let src = &self.inner[amt] as *const u8;
                let dst = &mut self.inner[0] as *mut u8;
                std::ptr::copy(src, dst, new_len)
            };
        }
        self.len = new_len;
        self.len += read.read(&mut self.inner[self.len..]).unwrap();
    }

    fn buf(&self) -> &[u8] {
        &self.inner[0..self.len]
    }
}

#[derive(Debug)]
enum State {
    Start,
    FoundBoundary(usize),
    ReceivingFile(usize, TmpFile),
}

impl State {
    fn new() -> Self {
        Self::Start
    }
}

#[derive(Debug)]
struct TmpFile {
    path: PathBuf,
    file: File,
}

impl TmpFile {
    fn finalize(self, options: &Args) {
        move_file(
            &self.path,
            PathBuf::from(&options.output_dir).join(self.path.file_name().unwrap()),
            &CopyOptions {
                overwrite: true,
                skip_exist: false,
                buffer_size: 64000,
            },
        )
        .unwrap();
    }
}

fn handle_request(
    mut body: impl Read,
    boundary: &str,
    _content_len: usize,
    writer: TcpStream,
    options: &Args,
) {
    let tmp_dir = PathBuf::from("/tmp/uploadd");
    if !tmp_dir.exists() {
        create_dir(&tmp_dir).unwrap();
    }

    let boundary = format!("\r\n--{boundary}");
    dbg!(&boundary);

    let mut buffer = Buf::with_capacity(1024);
    buffer.consume_and_read(0, &mut body);
    let mut state = State::new();

    loop {
        let buf = buffer.buf();
        if !matches!(state, State::ReceivingFile(_, _)) {
            dbg!(&state);
        }
        state = match state {
            State::Start => {
                if let Some(next_boundary) = memmem::find(buf, boundary.as_bytes()) {
                    State::FoundBoundary(next_boundary + boundary.len())
                } else {
                    buffer.consume_and_read(0, &mut body);
                    State::Start
                }
            }
            State::FoundBoundary(boundary_end) => {
                let remaining = &buf[boundary_end..];

                if remaining.len() > 2 && remaining.starts_with(b"--") {
                    // close file
                    break;
                }

                if let Some(headers_end) = memmem::find(remaining, b"\r\n\r\n") {
                    let headers = remaining[2..headers_end].lines();
                    let filename = parse_file_name(headers);
                    dbg!(&filename);
                    if filename.is_empty() {
                        break;
                    }
                    let path = tmp_dir.clone().join(filename);
                    let file = File::create(&path).unwrap();
                    let tmp_file = TmpFile { path, file };
                    State::ReceivingFile(boundary_end + headers_end + b"\r\n\r\n".len(), tmp_file)
                } else {
                    assert!(boundary_end != 0);
                    buffer.consume_and_read(boundary_end, &mut body);
                    State::FoundBoundary(0)
                }
            }
            State::ReceivingFile(start, mut tmp_file) => {
                let remaining = &buf[start..];
                if let Some(next_boundary) = memmem::find(remaining, boundary.as_bytes()) {
                    let boundary_end = next_boundary + boundary.len();
                    tmp_file
                        .file
                        .write_all(&remaining[..next_boundary])
                        .unwrap();
                    tmp_file.finalize(options);
                    State::FoundBoundary(boundary_end + start)
                } else {
                    let consume_amt = if remaining.len() + 1 >= boundary.len() {
                        let suffix = &remaining[remaining.len() + 1 - boundary.len()..];

                        if let Some(maybe_next_boundary) = (0..suffix.len() - 1)
                            .find(|&start| is_prefix(boundary.as_bytes(), &suffix[start..]))
                        {
                            dbg!(maybe_next_boundary);
                            let consume_len = buf.len() - suffix.len() + maybe_next_boundary;
                            let suspected_boundary = std::str::from_utf8(&buf[consume_len..]);
                            dbg!(suspected_boundary.unwrap());
                            consume_len
                        } else {
                            buf.len()
                        }
                    } else {
                        buf.len()
                    };
                    tmp_file.file.write_all(&buf[start..consume_amt]).unwrap();
                    buffer.consume_and_read(consume_amt, &mut body);
                    State::ReceivingFile(0, tmp_file)
                }
            }
        };
    }

    let mut writer = BufWriter::new(writer);
    write!(
        writer,
        "HTTP/1.1 303 See Other\r\nLocation: ../upload\r\n\r\n"
    )
    .unwrap();
    drop(writer);

    if let Some(shell_cmd) = &options.cmd {
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(shell_cmd);
        cmd.spawn().unwrap();
    }
}

fn parse_file_name(headers: std::io::Lines<&[u8]>) -> String {
    let mut filename = None;
    for header in headers {
        let header = header.unwrap();
        let (name, value) = header.split_once(':').unwrap();
        let name = name.trim();
        match name {
            "Content-Disposition" => {
                let filename_param_name = "filename";
                let filename_param_start = value.find(filename_param_name).unwrap();
                let remaining_value = &value[filename_param_start..];
                let filename_start = remaining_value.find('"').unwrap() + 1;
                let filename_end = remaining_value[filename_start..].find('"').unwrap();
                filename = Some(
                    remaining_value[filename_start..filename_start + filename_end].to_string(),
                );
            }
            "Content-Type" => {}
            _ => {}
        }
    }

    filename.unwrap()
}

#[cfg(test)]
mod test {
    use std::io::Cursor;

    use super::Buf;

    #[test]
    fn test_buf() {
        let mut buf = Buf::with_capacity(4);
        let read = [0, 1, 2, 3, 4, 5, 6, 7];
        let mut cursor = Cursor::new(read);
        buf.consume_and_read(0, &mut cursor);
        assert_eq!(buf.buf(), &[0, 1, 2, 3]);
        buf.consume_and_read(3, &mut cursor);
        assert_eq!(buf.buf(), &[3, 4, 5, 6]);
    }
}
