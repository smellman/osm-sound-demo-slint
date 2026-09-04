//! A `Read + Seek` view over an HTTP body that is still arriving.
//!
//! rodio's decoder needs `Read + Seek`, so the obvious thing is to download the
//! whole track and hand over a `Cursor` — but then playback only starts once
//! the last byte has landed, which for a several-megabyte MP3 is a visible
//! wait. Instead the bytes fetched so far are kept in memory and a read past
//! the write head blocks until the downloader catches up, so decoding starts as
//! soon as there is enough to probe.
//!
//! Everything fetched is retained, which is what makes `Seek` work: symphonia
//! rewinds over the first few kilobytes while identifying the stream. The
//! decoder is told the stream is *not* seekable (see [`crate::audio`]), so it
//! never tries to seek to the end and stall on the whole file.

use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Ceiling on what one track may buffer. Archive.org MP3s are a few MB, so this
/// is a runaway guard rather than an expected size.
const MAX_BYTES: usize = 96 * 1024 * 1024;

/// How much to read from the socket at a time.
const CHUNK: usize = 32 * 1024;

/// How long a read may wait for the downloader before giving up.
///
/// This matters because the decoder is pulled from the audio device's callback
/// thread: blocking there is what a dropout sounds like, so a stalled download
/// ends the track — and the app moves to the next one — rather than wedging
/// playback. The buffer normally runs far ahead of the playhead, since the body
/// is fetched as fast as the network allows rather than in real time.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug)]
struct Shared {
    state: Mutex<State>,
    progress: Condvar,
}

#[derive(Debug)]
struct State {
    bytes: Vec<u8>,
    /// Total length from `Content-Length`, when the server sent one.
    byte_len: Option<u64>,
    /// The body ended, successfully or not.
    finished: bool,
    failure: Option<String>,
    /// The reader went away, so there is no point fetching more.
    abandoned: bool,
}

impl Shared {
    fn finish(&self, failure: Option<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.finished = true;
            if state.failure.is_none() {
                state.failure = failure;
            }
        }
        self.progress.notify_all();
    }
}

/// Creates a reader and the writer that feeds it.
pub fn channel(byte_len: Option<u64>) -> (StreamingRead, StreamingWrite) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            bytes: Vec::new(),
            byte_len,
            finished: false,
            failure: None,
            abandoned: false,
        }),
        progress: Condvar::new(),
    });
    (
        StreamingRead {
            shared: Arc::clone(&shared),
            position: 0,
        },
        StreamingWrite { shared },
    )
}

/// The reading half, handed to the decoder.
#[derive(Debug)]
pub struct StreamingRead {
    shared: Arc<Shared>,
    position: u64,
}

impl StreamingRead {
    /// Total length, if the server declared one.
    pub fn byte_len(&self) -> Option<u64> {
        self.shared
            .state
            .lock()
            .ok()
            .and_then(|state| state.byte_len)
    }

    /// How much has been buffered so far.
    #[cfg(test)]
    pub fn buffered(&self) -> usize {
        self.shared
            .state
            .lock()
            .map(|state| state.bytes.len())
            .unwrap_or(0)
    }

    /// Blocks until `bytes` have been buffered, or the body ends first.
    ///
    /// Called before handing the reader to the player so playback opens with a
    /// cushion instead of stuttering on the first frames.
    pub fn wait_for(&self, bytes: usize) -> Result<(), String> {
        let Ok(mut state) = self.shared.state.lock() else {
            return Err("the download thread panicked".to_owned());
        };
        loop {
            if let Some(failure) = &state.failure {
                return Err(failure.clone());
            }
            if state.finished || state.bytes.len() >= bytes {
                return Ok(());
            }
            let Ok(next) = self.shared.progress.wait(state) else {
                return Err("the download thread panicked".to_owned());
            };
            state = next;
        }
    }
}

impl Read for StreamingRead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut state = self
            .shared
            .state
            .lock()
            .map_err(|_| io::Error::other("the download thread panicked"))?;
        loop {
            let available = state.bytes.len() as u64;
            if self.position < available {
                let start = self.position as usize;
                let take = out.len().min(state.bytes.len() - start);
                out[..take].copy_from_slice(&state.bytes[start..start + take]);
                self.position += take as u64;
                return Ok(take);
            }
            if let Some(failure) = &state.failure {
                return Err(io::Error::other(failure.clone()));
            }
            // Caught up with the write head: either that is the end of the
            // track, or the downloader has more on the way.
            if state.finished {
                return Ok(0);
            }
            let (next, timed_out) = self
                .shared
                .progress
                .wait_timeout(state, READ_TIMEOUT)
                .map_err(|_| io::Error::other("the download thread panicked"))?;
            if timed_out.timed_out() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "the track stopped arriving",
                ));
            }
            state = next;
        }
    }
}

impl Seek for StreamingRead {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        // Seeking only moves the cursor; a read past the write head is what
        // blocks. Seeking relative to the end needs the length, which is only
        // known from `Content-Length` or once the body has ended.
        let target = match from {
            SeekFrom::Start(offset) => offset as i64,
            SeekFrom::Current(offset) => self.position as i64 + offset,
            SeekFrom::End(offset) => {
                let end = {
                    let state = self
                        .shared
                        .state
                        .lock()
                        .map_err(|_| io::Error::other("the download thread panicked"))?;
                    match state.byte_len {
                        Some(len) => len,
                        None if state.finished => state.bytes.len() as u64,
                        None => {
                            return Err(io::Error::other(
                                "cannot seek from the end of a stream of unknown length",
                            ));
                        }
                    }
                };
                end as i64 + offset
            }
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "cannot seek before the start of the stream",
            ));
        }
        self.position = target as u64;
        Ok(self.position)
    }
}

impl Drop for StreamingRead {
    fn drop(&mut self) {
        if let Ok(mut state) = self.shared.state.lock() {
            state.abandoned = true;
        }
        self.shared.progress.notify_all();
    }
}

/// The writing half, which pumps the HTTP body into the buffer.
pub struct StreamingWrite {
    shared: Arc<Shared>,
}

impl StreamingWrite {
    /// Reads `body` to its end, appending as it goes. Returns early once the
    /// reader has been dropped, so skipping a track stops its download.
    pub fn pump(self, mut body: impl Read) {
        let mut chunk = vec![0u8; CHUNK];
        loop {
            match body.read(&mut chunk) {
                Ok(0) => return self.shared.finish(None),
                Ok(read) => {
                    let Ok(mut state) = self.shared.state.lock() else {
                        return;
                    };
                    if state.abandoned {
                        return;
                    }
                    if state.bytes.len() + read > MAX_BYTES {
                        drop(state);
                        return self
                            .shared
                            .finish(Some(format!("the track is larger than {MAX_BYTES} bytes")));
                    }
                    state.bytes.extend_from_slice(&chunk[..read]);
                    drop(state);
                    self.shared.progress.notify_all();
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return self.shared.finish(Some(error.to_string())),
            }
        }
    }
}

impl Drop for StreamingWrite {
    fn drop(&mut self) {
        // A reader blocked on the condvar must not be left waiting if the pump
        // unwinds or returns without saying so.
        self.shared.finish(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_returns_what_has_arrived_so_far() {
        let (mut reader, writer) = channel(Some(6));
        std::thread::spawn(move || writer.pump(&b"abcdef"[..]));

        let mut all = Vec::new();
        reader.read_to_end(&mut all).expect("reads to the end");
        assert_eq!(all, b"abcdef");
        assert_eq!(reader.byte_len(), Some(6));
    }

    #[test]
    fn a_read_past_the_write_head_waits_for_the_downloader() {
        let (mut reader, writer) = channel(None);
        let pump = std::thread::spawn(move || {
            // Feed the second half only after the reader has had to wait.
            writer.pump(SlowBody::new(vec![b"first".to_vec(), b"second".to_vec()]));
        });

        let mut all = Vec::new();
        reader.read_to_end(&mut all).expect("reads to the end");
        pump.join().expect("the pump finishes");
        assert_eq!(all, b"firstsecond");
    }

    #[test]
    fn seeking_backwards_rereads_buffered_bytes() {
        let (mut reader, writer) = channel(Some(6));
        std::thread::spawn(move || writer.pump(&b"abcdef"[..]));
        reader.wait_for(6).expect("the body arrives");

        let mut head = [0u8; 3];
        reader.read_exact(&mut head).expect("reads the head");
        assert_eq!(&head, b"abc");

        // Symphonia rewinds over the start while identifying the stream.
        assert_eq!(reader.seek(SeekFrom::Start(1)).expect("seeks"), 1);
        reader.read_exact(&mut head).expect("reads again");
        assert_eq!(&head, b"bcd");

        // And relative to the end, which needs the declared length.
        assert_eq!(reader.seek(SeekFrom::End(-2)).expect("seeks"), 4);
        let mut tail = [0u8; 2];
        reader.read_exact(&mut tail).expect("reads the tail");
        assert_eq!(&tail, b"ef");
    }

    #[test]
    fn seeking_from_the_end_of_an_unsized_stream_is_refused_until_it_ends() {
        let (mut reader, writer) = channel(None);
        assert!(reader.seek(SeekFrom::End(0)).is_err());

        writer.pump(&b"abcd"[..]);
        // Once the body has ended its length is known after all.
        assert_eq!(reader.seek(SeekFrom::End(0)).expect("seeks"), 4);
    }

    #[test]
    fn a_failed_download_surfaces_as_a_read_error() {
        let (mut reader, writer) = channel(None);
        std::thread::spawn(move || writer.pump(FailingBody));

        let mut all = Vec::new();
        let error = reader.read_to_end(&mut all).expect_err("the read fails");
        assert!(error.to_string().contains("no carrier"), "{error}");
    }

    #[test]
    fn dropping_the_reader_stops_the_download() {
        let (reader, writer) = channel(None);
        let endless = std::thread::spawn(move || {
            writer.pump(EndlessBody);
        });

        // Let it get going, then abandon it as skipping a track would.
        std::thread::sleep(Duration::from_millis(20));
        drop(reader);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !endless.is_finished() {
            assert!(
                std::time::Instant::now() < deadline,
                "the pump kept running after the reader was dropped"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn waiting_for_more_than_the_body_holds_returns_when_it_ends() {
        let (reader, writer) = channel(None);
        std::thread::spawn(move || writer.pump(&b"short"[..]));
        reader
            .wait_for(1024)
            .expect("returns at the end of the body");
    }

    /// Hands over one piece per read, with a pause first, so a reader has to
    /// wait on the condvar rather than finding everything already buffered.
    struct SlowBody {
        pieces: std::vec::IntoIter<Vec<u8>>,
    }

    impl SlowBody {
        fn new(pieces: Vec<Vec<u8>>) -> Self {
            Self {
                pieces: pieces.into_iter(),
            }
        }
    }

    impl Read for SlowBody {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(10));
            match self.pieces.next() {
                Some(piece) => {
                    let take = out.len().min(piece.len());
                    out[..take].copy_from_slice(&piece[..take]);
                    Ok(take)
                }
                None => Ok(0),
            }
        }
    }

    struct FailingBody;

    impl Read for FailingBody {
        fn read(&mut self, _out: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("no carrier"))
        }
    }

    struct EndlessBody;

    impl Read for EndlessBody {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(5));
            out.fill(0);
            Ok(out.len())
        }
    }

    #[test]
    fn a_stalled_download_ends_the_track_instead_of_blocking_the_audio_thread() {
        let (mut reader, writer) = channel(None);
        // Hold the writer without ever feeding it, so the reader has nothing
        // to read and no end-of-body either.
        let stall = std::thread::spawn(move || {
            std::thread::sleep(READ_TIMEOUT + Duration::from_secs(5));
            drop(writer);
        });

        let started = std::time::Instant::now();
        let mut buf = [0u8; 16];
        let error = reader.read(&mut buf).expect_err("the read times out");
        let waited = started.elapsed();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(waited >= READ_TIMEOUT, "returned after only {waited:?}");
        assert!(
            waited < READ_TIMEOUT + Duration::from_secs(3),
            "waited {waited:?}"
        );
        drop(reader);
        stall.join().expect("the stall thread finishes");
    }
}
