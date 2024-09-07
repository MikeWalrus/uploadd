#![feature(core_io_borrowed_buf)]
#![feature(read_buf)]
#![feature(let_chains)]
#![feature(new_uninit)]
#![feature(maybe_uninit_slice)]
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    str::FromStr,
};

use httparse::{Header, Request};
use memchr::{arch::all::is_prefix, memmem};

fn main() {
    let listener = TcpListener::bind("127.0.0.1:12141").unwrap();

    for stream in listener.incoming() {
        let stream = stream.unwrap();
        handle_connection(stream);
        println!("Connection established!");
    }
}

fn find_header_value<T>(headers: &[Header], name: &str) -> T
where
    T: FromStr,
    T::Err: std::fmt::Debug,
{
    let header = headers.iter().find(|h| h.name == name).unwrap();
    std::str::from_utf8(header.value)
        .unwrap()
        .parse::<T>()
        .unwrap()
}

fn handle_connection(stream: TcpStream) {
    let reader = stream.try_clone().unwrap();
    let writer = stream;
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
            eprintln!("is complete");
            let header_len = result.unwrap();
            let content_len: usize = find_header_value(&headers, "Content-Length");
            let content_type: String = find_header_value(&headers, "Content-Type");
            let boundary = parse_boundary(&content_type);
            dbg!(&content_type);
            let body_received = &buf[header_len - b"\r\n".len()..];
            // dbg!(std::str::from_utf8(&body_received[..64]));
            let body = body_received.chain(reader.into_inner());
            handle_request(body, boundary, content_len, writer);
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
    ReceivingFile(usize),
}

impl State {
    fn new() -> Self {
        Self::Start
    }
}

fn handle_request(mut body: impl Read, boundary: &str, _content_len: usize, mut writer: TcpStream) {
    let boundary = format!("\r\n--{boundary}");
    dbg!(&boundary);

    let mut buffer = Buf::with_capacity(65536);
    buffer.consume_and_read(0, &mut body);
    let mut state = State::new();

    loop {
        let buf = buffer.buf();
        if !matches!(state, State::ReceivingFile(_)) {
            dbg!(&state);
        }
        state = match state {
            State::Start => {
                // dbg!(std::str::from_utf8(&buf[..30]));
                let next_boundary = memmem::find(buf, boundary.as_bytes()).unwrap();
                State::FoundBoundary(next_boundary + boundary.len())
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
                    State::ReceivingFile(boundary_end + headers_end)
                } else {
                    assert!(boundary_end != 0);
                    buffer.consume_and_read(boundary_end, &mut body);
                    State::FoundBoundary(0)
                }
            }
            State::ReceivingFile(start) => {
                let remaining = &buf[start..];
                if let Some(next_boundary) = memmem::find(remaining, boundary.as_bytes()) {
                    let boundary_end = next_boundary + boundary.len();
                    // write file
                    // dbg!(std::str::from_utf8(
                    //     &remaining[boundary_end..boundary_end + 30]
                    // ));
                    // buffer.consume_and_read(boundary_end + start, &mut body);
                    State::FoundBoundary(boundary_end + start)
                } else {
                    let consume_amt = if remaining.len() + 1 >= boundary.len() {
                        let suffix = &remaining[remaining.len() + 1 - boundary.len()..];

                        if let Some(maybe_next_boundary) = (0..suffix.len() - 1)
                            .find(|&start| is_prefix(boundary.as_bytes(), &suffix[start..]))
                        {
                            // write until that
                            dbg!(maybe_next_boundary);
                            let consume_len = buf.len() - suffix.len() + maybe_next_boundary;
                            let suspected_boundary = std::str::from_utf8(&buf[consume_len..]);
                            dbg!(suspected_boundary.unwrap());
                            consume_len
                        } else {
                            // write to the end
                            buf.len()
                        }
                    } else {
                        // write to the end
                        buf.len()
                    };
                    buffer.consume_and_read(consume_amt, &mut body);
                    State::ReceivingFile(0)
                }
            }
        };

        /*
        let mut boundaries = memmem::find_iter(buf, boundary.as_bytes());

        let mut remaining;
        let boundary_found;
        if let Some(this_boundary) = boundaries.next() {
            let this_boundary_end = this_boundary + boundary.len();
            dbg!(std::str::from_utf8(
                &buf[this_boundary..this_boundary + 120]
            ));
            if buf[this_boundary_end..].starts_with(b"--") {
                // write and close file
                break;
            }
            let part_start = this_boundary_end + b"\r\n".len();
            const MAX_HEADER_LEN: usize = 1024;
            let headers = &buf[part_start..];
            let headers_end = memmem::find(headers, b"\r\n\r\n").unwrap();
            let headers = headers[..headers_end].lines();
            let filename = parse_file_name(headers);
            dbg!(&filename);
            remaining = &buf[headers_end + b"\r\n\r\n".len()..];
            boundary_found = true;
        } else {
            remaining = buf;
            boundary_found = false;
        };

        let consume_amt = if boundary_found && let Some(next_boundary) = boundaries.next() {
            // write until boundary
            dbg!(next_boundary);
            next_boundary
        } else {
            // check buffer suffix
            //dbg!(remaining.len());
            let suffix = &remaining[remaining.len() - boundary.len() + 1..];

            if let Some(maybe_next_boundary) = (0..suffix.len() - 1)
                .find(|&start| is_prefix(boundary.as_bytes(), &suffix[start..]))
            {
                // write until that
                dbg!(maybe_next_boundary);
                let consume_len = buf.len() - suffix.len() + maybe_next_boundary;
                dbg!(std::str::from_utf8(&buf[consume_len..]));
                consume_len
            } else {
                // write to the end
                buf.len()
            }
        };
        //dbg!(consume_amt);
        // buffer.consume_and_read(consume_amt, &mut body);
        */
    }

    let response = "HTTP/1.1 200 OK\r\n\r\nUploaded.";
    writer.write_all(response.as_bytes()).unwrap();
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
