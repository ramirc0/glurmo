//! A reader for server-sent events, fed the response body chunk by chunk.

/// One event off the stream.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The event's type, `message` unless the server named one.
    pub kind: String,
    /// The data lines, joined by newlines.
    pub data: String,
    /// The event's ID, if it carried one.
    pub id: Option<String>,
}

/// Splits a byte stream into events. A chunk can end anywhere, even inside a UTF-8 character.
#[derive(Default)]
pub struct SseReader {
    pending: Vec<u8>,
    event: SseEvent,
    has_data: bool,
}

impl SseReader {
    /// Reads one chunk and returns the events it completed.
    pub fn read(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.pending.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(end) = self.pending.iter().position(|&byte| byte == b'\n') {
            let line: Vec<u8> = self.pending.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if self.has_data {
                    let mut event = std::mem::take(&mut self.event);
                    if event.kind.is_empty() {
                        "message".clone_into(&mut event.kind);
                    }
                    events.push(event);
                }
                self.event = SseEvent::default();
                self.has_data = false;
                continue;
            }
            let (field, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.strip_prefix(' ').unwrap_or(value);
            match field {
                "event" => value.clone_into(&mut self.event.kind),
                "data" => {
                    if self.has_data {
                        self.event.data.push('\n');
                    }
                    self.event.data.push_str(value);
                    self.has_data = true;
                }
                "id" => self.event.id = Some(value.to_owned()),
                _ => {}
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: &str, data: &str, id: Option<&str>) -> SseEvent {
        SseEvent {
            kind: kind.into(),
            data: data.into(),
            id: id.map(Into::into),
        }
    }

    #[test]
    fn events_split_on_blank_lines_whatever_the_chunks() {
        let stream = "retry: 5000\nid: 9-1\nevent: snapshot\ndata: {\"a\":1}\n\n: keepalive\n\n\
                      event: delta\r\nid: 9-2\r\ndata: x\r\ndata: y\r\n\r\ndata:plain\n\n";
        let expected = [
            event("snapshot", "{\"a\":1}", Some("9-1")),
            event("delta", "x\ny", Some("9-2")),
            event("message", "plain", None),
        ];
        assert_eq!(SseReader::default().read(stream.as_bytes()), expected);
        for size in [1, 2, 7] {
            let mut reader = SseReader::default();
            let events: Vec<SseEvent> = stream
                .as_bytes()
                .chunks(size)
                .flat_map(|chunk| reader.read(chunk))
                .collect();
            assert_eq!(events, expected, "chunks of {size}");
        }
    }

    #[test]
    fn a_character_split_across_chunks_survives() {
        let bytes = "data: Rosé\n\n".as_bytes();
        let mut reader = SseReader::default();
        assert_eq!(reader.read(&bytes[..9]), []);
        assert_eq!(reader.read(&bytes[9..])[0].data, "Rosé");
    }
}
