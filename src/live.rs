//! Live dictation: microphone audio streamed to a speech server over a
//! WebSocket, with the words coming back while you talk.
//!
//! The protocol is the Nemotron ASR stream (`/v1/stream`): binary frames of
//! 16 kHz signed 16-bit mono PCM go up; JSON `partial` (the utterance so
//! far) and `final` (one finished utterance) events come back. `eos` ends
//! the session after a last `final`.
//!
//! The WebSocket client is a minimal one for plain `ws://` on a private
//! network (no TLS, no extensions): enough for this protocol, with masked
//! client frames as the standard requires.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::{Child, ChildStdout};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::services::ServiceEvent;

/// PCM sent per frame: 100 ms at 16 kHz, 16-bit mono.
const FRAME_BYTES: usize = 3200;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Longest server message accepted (they are small JSON events).
const MAX_MESSAGE: u64 = 1 << 20;

/// The stream URL for a dictation base url: `http://h:p/v1` becomes
/// `ws://h:p/v1/stream`. `None` for anything but plain http (no TLS here).
pub fn stream_url(base: &str) -> Option<String> {
    let base = base.trim().trim_end_matches('/');
    let rest = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("ws://"))?;
    if rest.is_empty() {
        return None;
    }
    let path = if rest.ends_with("/stream") {
        ""
    } else {
        "/stream"
    };
    Some(format!("ws://{rest}{path}"))
}

struct Url {
    host: String,
    port: u16,
    path: String,
}

fn parse_ws_url(url: &str) -> io::Result<Url> {
    let rest = url
        .strip_prefix("ws://")
        .ok_or_else(|| io::Error::other("only ws:// urls are supported"))?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) if !host.ends_with(']') || authority.starts_with('[') => (
            host,
            port.parse::<u16>()
                .map_err(|_| io::Error::other("bad port in the stream url"))?,
        ),
        _ => (authority, 80),
    };
    if host.is_empty() {
        return Err(io::Error::other("no host in the stream url"));
    }
    Ok(Url {
        host: host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_owned(),
        port,
        path: path.to_owned(),
    })
}

/// Bytes that only need to differ between connections (the handshake key
/// and frame masks are not secrets).
fn noise(counter: &AtomicU64) -> [u8; 16] {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.as_nanos() as u64);
    let mut state = nanos
        ^ u64::from(std::process::id()) << 32
        ^ counter.fetch_add(0x9e37_79b9_7f4a_7c15, Ordering::Relaxed);
    let mut out = [0u8; 16];
    for chunk in out.chunks_mut(8) {
        // xorshift64*
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let value = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
        chunk.copy_from_slice(&value.to_le_bytes()[..chunk.len()]);
    }
    out
}

static NOISE: AtomicU64 = AtomicU64::new(1);

/// An open WebSocket: a buffered reader and a shared writer.
pub struct Socket {
    pub reader: BufReader<TcpStream>,
    pub writer: Arc<Mutex<TcpStream>>,
}

/// Opens `url` and completes the WebSocket handshake.
pub fn connect(url: &str) -> io::Result<Socket> {
    let url = parse_ws_url(url)?;
    let address = (url.host.as_str(), url.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::other("the stream host did not resolve"))?;
    let stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(CONNECT_TIMEOUT))?;
    let key = crate::text::base64(&noise(&NOISE));
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: {}:{}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n",
        url.path, url.host, url.port
    );
    let mut writer = stream.try_clone()?;
    writer.write_all(request.as_bytes())?;
    let mut reader = BufReader::new(stream);
    let mut status = String::new();
    reader.read_line(&mut status)?;
    if !status.starts_with("HTTP/1.1 101") {
        return Err(io::Error::other(format!(
            "the speech server refused the stream: {}",
            status.trim()
        )));
    }
    // Skip the rest of the response headers.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::other("the stream closed during the handshake"));
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
    }
    reader.get_ref().set_read_timeout(None)?;
    Ok(Socket {
        reader,
        writer: Arc::new(Mutex::new(writer)),
    })
}

pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
const OP_CONTINUATION: u8 = 0x0;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// Encodes one masked client frame (FIN set).
pub fn encode_frame(opcode: u8, payload: &[u8], mask: [u8; 4]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | (opcode & 0x0f));
    let len = payload.len();
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if len <= usize::from(u16::MAX) {
        frame.push(0x80 | 126);
        frame.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        frame.push(0x80 | 127);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(i, byte)| byte ^ mask[i % 4]),
    );
    frame
}

fn send(writer: &Mutex<TcpStream>, opcode: u8, payload: &[u8]) -> io::Result<()> {
    let bytes = noise(&NOISE);
    let frame = encode_frame(opcode, payload, [bytes[0], bytes[1], bytes[2], bytes[3]]);
    let mut stream = match writer.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    stream.write_all(&frame)
}

/// Reads one whole message (joining fragments): its opcode and payload.
/// Pings are answered here and never returned.
pub fn read_message(
    reader: &mut impl Read,
    writer: &Mutex<TcpStream>,
) -> io::Result<(u8, Vec<u8>)> {
    let mut message: Option<(u8, Vec<u8>)> = None;
    loop {
        let mut head = [0u8; 2];
        reader.read_exact(&mut head)?;
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0f;
        let masked = head[1] & 0x80 != 0;
        let mut len = u64::from(head[1] & 0x7f);
        if len == 126 {
            let mut ext = [0u8; 2];
            reader.read_exact(&mut ext)?;
            len = u64::from(u16::from_be_bytes(ext));
        } else if len == 127 {
            let mut ext = [0u8; 8];
            reader.read_exact(&mut ext)?;
            len = u64::from_be_bytes(ext);
        }
        if len > MAX_MESSAGE {
            return Err(io::Error::other("stream message too large"));
        }
        let mut mask = [0u8; 4];
        if masked {
            reader.read_exact(&mut mask)?;
        }
        let mut payload = vec![0u8; len as usize];
        reader.read_exact(&mut payload)?;
        if masked {
            for (i, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[i % 4];
            }
        }
        match opcode {
            OP_PING => {
                send(writer, OP_PONG, &payload)?;
                continue;
            }
            OP_PONG => continue,
            OP_CLOSE => return Ok((OP_CLOSE, payload)),
            OP_CONTINUATION => match message.as_mut() {
                Some((_, data)) => data.extend_from_slice(&payload),
                None => return Err(io::Error::other("stray continuation frame")),
            },
            _ => message = Some((opcode, payload)),
        }
        if fin {
            if let Some(done) = message.take() {
                return Ok(done);
            }
        }
    }
}

/// A live dictation in progress.
#[derive(Debug)]
pub struct Live {
    pub job: u64,
    pub started: Instant,
    /// Set once the user asked to stop (waiting for the last words).
    pub stopping: Option<Instant>,
    recorder: Arc<Mutex<Option<Child>>>,
    done: Arc<AtomicBool>,
    socket: Option<Arc<Mutex<TcpStream>>>,
}

impl Live {
    /// A session with nothing behind it (tests drive it with events).
    #[cfg(test)]
    pub fn detached(job: u64) -> Live {
        Live {
            job,
            started: Instant::now(),
            stopping: None,
            recorder: Arc::new(Mutex::new(None)),
            done: Arc::new(AtomicBool::new(false)),
            socket: None,
        }
    }

    /// Stops the microphone; the last words still arrive, then the session
    /// ends with [`ServiceEvent::LiveEnded`].
    pub fn stop(&mut self, now: Instant) {
        if self.stopping.is_none() {
            self.stopping = Some(now);
            if let Some(child) = lock(&self.recorder).as_ref() {
                interrupt(child);
            }
        }
    }

    /// Ends everything now, without waiting for the last words.
    pub fn abort(&self) {
        self.done.store(true, Ordering::SeqCst);
        if let Some(mut child) = lock(&self.recorder).take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Unblocks the thread reading server events.
        if let Some(socket) = &self.socket {
            let _ = lock(socket).shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.abort();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(unix)]
fn interrupt(child: &Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: kill(2) on our own child's pid; no memory is touched.
        unsafe {
            libc::kill(pid, libc::SIGINT);
        }
    }
}

#[cfg(not(unix))]
fn interrupt(_child: &Child) {}

/// Connects to `url`, then streams `recorder`'s stdout (raw PCM) to it on
/// one thread and turns the server's events into [`ServiceEvent`]s on
/// another.
pub fn start(
    url: &str,
    mut recorder: Child,
    job: u64,
    tx: Sender<ServiceEvent>,
) -> Result<Live, String> {
    let socket = match connect(url) {
        Ok(socket) => socket,
        Err(err) => {
            let _ = recorder.kill();
            let _ = recorder.wait();
            return Err(format!("live dictation: {err}"));
        }
    };
    let Some(audio) = recorder.stdout.take() else {
        let _ = recorder.kill();
        return Err("the recorder gave no audio stream".to_owned());
    };
    let recorder = Arc::new(Mutex::new(Some(recorder)));
    let done = Arc::new(AtomicBool::new(false));
    let writer = Arc::clone(&socket.writer);
    {
        let done = Arc::clone(&done);
        let recorder = Arc::clone(&recorder);
        spawn("lumbridge-notes-live-audio", move || {
            pump_audio(audio, &writer, &done);
            // End of audio (stopped, or the recorder died): ask for the last
            // words and the close.
            let _ = send(&writer, OP_TEXT, br#"{"type":"eos"}"#);
            if let Some(mut child) = lock(&recorder).take() {
                let _ = child.wait();
            }
        });
    }
    {
        let done = Arc::clone(&done);
        let writer = Arc::clone(&socket.writer);
        let mut reader = socket.reader;
        spawn("lumbridge-notes-live-events", move || {
            let result = receive(&mut reader, &writer, job, &tx, &done);
            done.store(true, Ordering::SeqCst);
            if let Ok(stream) = lock(&writer).try_clone() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            let _ = tx.send(ServiceEvent::LiveEnded { job, result });
        });
    }
    Ok(Live {
        job,
        started: Instant::now(),
        stopping: None,
        recorder,
        done,
        socket: Some(socket.writer),
    })
}

fn spawn(name: &str, work: impl FnOnce() + Send + 'static) {
    let _ = std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(work);
}

fn pump_audio(mut audio: ChildStdout, writer: &Mutex<TcpStream>, done: &AtomicBool) {
    let mut buffer = vec![0u8; FRAME_BYTES];
    while !done.load(Ordering::SeqCst) {
        match audio.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                if send(writer, OP_BINARY, &buffer[..n]).is_err() {
                    return;
                }
            }
        }
    }
}

fn receive(
    reader: &mut impl Read,
    writer: &Mutex<TcpStream>,
    job: u64,
    tx: &Sender<ServiceEvent>,
    done: &AtomicBool,
) -> Result<(), String> {
    loop {
        let (opcode, payload) = match read_message(reader, writer) {
            Ok(message) => message,
            Err(_) if done.load(Ordering::SeqCst) => return Ok(()),
            Err(err) => return Err(format!("the stream broke: {err}")),
        };
        if opcode == OP_CLOSE {
            return Ok(());
        }
        if opcode != OP_TEXT {
            continue;
        }
        let Ok(event) = serde_json::from_slice::<Value>(&payload) else {
            continue;
        };
        let text = || event["text"].as_str().unwrap_or("").trim().to_owned();
        match event["type"].as_str() {
            Some("partial") => {
                let _ = tx.send(ServiceEvent::LivePartial { job, text: text() });
            }
            Some("final") => {
                let _ = tx.send(ServiceEvent::LiveFinal { job, text: text() });
            }
            Some("closed") => return Ok(()),
            Some("error") => {
                let message = event["message"].as_str().unwrap_or("error").to_owned();
                return Err(format!("speech server: {message}"));
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn stream_urls_come_from_the_dictation_url() {
        assert_eq!(
            stream_url("http://h:8791/v1").as_deref(),
            Some("ws://h:8791/v1/stream")
        );
        assert_eq!(
            stream_url("http://h:8791/v1/").as_deref(),
            Some("ws://h:8791/v1/stream")
        );
        assert_eq!(
            stream_url("ws://h/v1/stream").as_deref(),
            Some("ws://h/v1/stream")
        );
        assert_eq!(stream_url("https://h/v1"), None);
        assert_eq!(stream_url(""), None);
        let url = parse_ws_url("ws://192.0.2.1:8791/v1/stream?a=1").unwrap();
        assert_eq!(
            (url.host.as_str(), url.port, url.path.as_str()),
            ("192.0.2.1", 8791, "/v1/stream?a=1")
        );
        assert_eq!(parse_ws_url("ws://h").unwrap().port, 80);
    }

    #[test]
    fn frames_are_masked_and_sized() {
        let frame = encode_frame(OP_TEXT, b"hi", [1, 2, 3, 4]);
        assert_eq!(frame, [0x81, 0x82, 1, 2, 3, 4, b'h' ^ 1, b'i' ^ 2]);
        let big = encode_frame(OP_BINARY, &[0u8; 3200], [0; 4]);
        assert_eq!(&big[..4], &[0x82, 0x80 | 126, 0x0c, 0x80]);
        assert_eq!(big.len(), 4 + 4 + 3200);
    }

    /// A tiny server: completes the handshake, checks the audio arrives,
    /// sends a ping, a partial and (after `eos`) a fragmented final.
    #[test]
    fn talks_to_a_stream_server() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut writer = stream;
            let mut request = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(request.starts_with("GET /v1/stream HTTP/1.1"));
            assert!(request.contains("Sec-WebSocket-Key: "));
            writer
                .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
                .unwrap();
            let unmasked = |op: u8, fin: bool, data: &[u8]| {
                let mut frame = vec![if fin { 0x80 } else { 0 } | op, data.len() as u8];
                frame.extend_from_slice(data);
                frame
            };
            writer.write_all(&unmasked(OP_PING, true, b"p")).unwrap();
            writer
                .write_all(&unmasked(
                    OP_TEXT,
                    true,
                    br#"{"type":"partial","text":"hello"}"#,
                ))
                .unwrap();
            let shared = Mutex::new(writer.try_clone().unwrap());
            let mut audio = 0;
            loop {
                let (op, data) = read_message(&mut reader, &shared).unwrap();
                match op {
                    OP_BINARY => audio += data.len(),
                    OP_PONG => {}
                    OP_TEXT => {
                        assert_eq!(data, br#"{"type":"eos"}"#);
                        break;
                    }
                    other => panic!("unexpected opcode {other}"),
                }
            }
            writer
                .write_all(&unmasked(OP_TEXT, false, br#"{"type":"final","#))
                .unwrap();
            writer
                .write_all(&unmasked(
                    OP_CONTINUATION,
                    true,
                    br#""text":"hello world"}"#,
                ))
                .unwrap();
            writer
                .write_all(&unmasked(OP_TEXT, true, br#"{"type":"closed"}"#))
                .unwrap();
            audio
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let recorder = std::process::Command::new("sh")
            .args(["-c", "head -c 6400 /dev/zero"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let _live = start(&format!("ws://127.0.0.1:{port}/v1/stream"), recorder, 5, tx).unwrap();
        let mut events = Vec::new();
        while let Ok(event) = rx.recv_timeout(Duration::from_secs(5)) {
            let end = matches!(event, ServiceEvent::LiveEnded { .. });
            events.push(event);
            if end {
                break;
            }
        }
        assert_eq!(server.join().unwrap(), 6400);
        assert_eq!(
            events,
            [
                ServiceEvent::LivePartial {
                    job: 5,
                    text: "hello".into()
                },
                ServiceEvent::LiveFinal {
                    job: 5,
                    text: "hello world".into()
                },
                ServiceEvent::LiveEnded {
                    job: 5,
                    result: Ok(())
                },
            ]
        );
    }
}
