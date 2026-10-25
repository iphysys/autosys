//! Server-sent event framing, as the HTML standard describes it: lines end
//! in LF, CRLF or CR; a blank line ends a frame; a line starting with `:`
//! is a comment, such as the server's keep-alive. Only the fields API v0
//! sends are kept.

/// One frame: the server's `id`, `event` and `data` fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Frame {
    pub(crate) id: Option<String>,
    pub(crate) event: Option<String>,
    pub(crate) data: String,
}

/// Turns the bytes of a stream, in chunks of any size, into frames.
#[derive(Debug, Default)]
pub(crate) struct Parser {
    /// Bytes after the last line end.
    partial: Vec<u8>,
    /// The previous chunk ended in CR, so a leading LF is part of that line
    /// end.
    after_cr: bool,
    frame: Frame,
    /// A `data` field was seen in the current frame.
    has_data: bool,
}

impl Parser {
    /// The frames that `chunk` completes, in order.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<Frame> {
        let mut frames = Vec::new();
        let mut bytes = chunk;
        if self.after_cr {
            self.after_cr = false;
            if let Some(rest) = bytes.strip_prefix(b"\n") {
                bytes = rest;
            }
        }
        while let Some(end) = bytes.iter().position(|b| *b == b'\n' || *b == b'\r') {
            self.partial.extend_from_slice(&bytes[..end]);
            let line = std::mem::take(&mut self.partial);
            if let Some(frame) = self.line(&line) {
                frames.push(frame);
            }
            let cr = bytes[end] == b'\r';
            bytes = &bytes[end + 1..];
            if cr {
                match bytes.first() {
                    Some(b'\n') => bytes = &bytes[1..],
                    Some(_) => {}
                    None => self.after_cr = true,
                }
            }
        }
        self.partial.extend_from_slice(bytes);
        frames
    }

    fn line(&mut self, line: &[u8]) -> Option<Frame> {
        if line.is_empty() {
            let frame = std::mem::take(&mut self.frame);
            let dispatch = std::mem::take(&mut self.has_data);
            return dispatch.then_some(frame);
        }
        if line.starts_with(b":") {
            return None;
        }
        let line = String::from_utf8_lossy(line);
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line.as_ref(), ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.frame.data.push('\n');
                }
                self.frame.data.push_str(value);
                self.has_data = true;
            }
            "id" => self.frame.id = Some(value.to_owned()),
            "event" => self.frame.event = Some(value.to_owned()),
            _ => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(id: &str, event: &str, data: &str) -> Frame {
        Frame {
            id: Some(id.to_owned()),
            event: Some(event.to_owned()),
            data: data.to_owned(),
        }
    }

    const STREAM: &str = ": keep-alive\n\nid: 1\nevent: run_started\ndata: {\"seq\":1}\n\n\
                          id: 2\r\nevent: tool_call_started\r\ndata: {\"seq\":\r\ndata: 2}\r\n\r\n\
                          :comment\rid:3\revent:run_stopped\rdata:{}\r\r";

    fn expected() -> Vec<Frame> {
        vec![
            frame("1", "run_started", "{\"seq\":1}"),
            frame("2", "tool_call_started", "{\"seq\":\n2}"),
            frame("3", "run_stopped", "{}"),
        ]
    }

    #[test]
    fn frames_end_at_blank_lines_whatever_the_line_ends() {
        assert_eq!(Parser::default().feed(STREAM.as_bytes()), expected());
    }

    #[test]
    fn chunk_boundaries_do_not_matter() {
        let bytes = STREAM.as_bytes();
        for split in 0..=bytes.len() {
            let mut parser = Parser::default();
            let mut frames = parser.feed(&bytes[..split]);
            frames.extend(parser.feed(&bytes[split..]));
            assert_eq!(frames, expected(), "split at {split}");
        }
        let mut parser = Parser::default();
        let frames: Vec<Frame> = bytes.iter().flat_map(|b| parser.feed(&[*b])).collect();
        assert_eq!(frames, expected(), "one byte at a time");
    }

    #[test]
    fn an_unfinished_frame_is_not_dispatched() {
        let mut parser = Parser::default();
        assert!(parser.feed(b"id: 1\ndata: {}\n").is_empty());
        assert!(parser.feed(b"data: more").is_empty());
    }

    #[test]
    fn a_frame_without_data_is_dropped() {
        assert!(Parser::default().feed(b"id: 9\nevent: x\n\n").is_empty());
    }

    #[test]
    fn multibyte_text_split_across_chunks_survives() {
        let text = "data: \u{00b3}\u{00e9}\n\n".as_bytes();
        let mut parser = Parser::default();
        let mut frames = parser.feed(&text[..7]);
        frames.extend(parser.feed(&text[7..]));
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data, "\u{00b3}\u{00e9}");
    }
}
