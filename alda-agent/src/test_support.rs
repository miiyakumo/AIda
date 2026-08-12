use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

pub struct MockResponse {
    pub status: String,
    pub content_type: &'static str,
    pub body: String,
}

impl MockResponse {
    pub fn sse(body: String) -> Self {
        Self {
            status: "200 OK".to_string(),
            content_type: "text/event-stream",
            body,
        }
    }

    pub fn error(status: &str, body: &str) -> Self {
        Self {
            status: status.to_string(),
            content_type: "text/plain",
            body: body.to_string(),
        }
    }
}

pub fn serve(responses: Vec<MockResponse>) -> (String, mpsc::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for response in responses {
            let (mut stream, _) = listener.accept().unwrap();
            let request = read_request(&mut stream);
            let _ = sender.send(request);
            let wire = format!(
                "HTTP/1.1 {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response.status,
                response.content_type,
                response.body.len(),
                response.body
            );
            let split = wire.len() / 2;
            stream.write_all(&wire.as_bytes()[..split]).unwrap();
            stream.write_all(&wire.as_bytes()[split..]).unwrap();
        }
    });
    (format!("http://{address}"), receiver)
}

fn read_request(stream: &mut impl Read) -> Vec<u8> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).unwrap();
        request.extend_from_slice(&buffer[..read]);
        if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let read = stream.read(&mut buffer).unwrap();
        request.extend_from_slice(&buffer[..read]);
    }
    request
}
