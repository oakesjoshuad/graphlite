//! Thin helpers over quick-xml for the attribute-heavy XML patterns used
//! throughout graphlite output.  All helpers escape values automatically.

use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

#[allow(dead_code)]
pub type XmlWriter = Writer<Vec<u8>>;
pub type StreamXmlWriter = Writer<std::io::BufWriter<std::io::Stdout>>;

/// Create a new indented writer (2 spaces).
#[allow(dead_code)]
pub fn new_writer() -> XmlWriter {
    Writer::new_with_indent(Vec::new(), b' ', 2)
}

/// Create a streaming writer to stdout with buffered IO.
pub fn new_stream_writer() -> StreamXmlWriter {
    Writer::new_with_indent(std::io::BufWriter::new(std::io::stdout()), b' ', 2)
}

/// Consume the writer and return the UTF-8 string.
#[allow(dead_code)]
pub fn finish(w: XmlWriter) -> String {
    String::from_utf8(w.into_inner()).expect("xml output is always valid utf-8")
}

/// Flush and finish a stdout-backed writer.
pub fn finish_stream(w: StreamXmlWriter) -> anyhow::Result<()> {
    use std::io::Write;
    let mut inner = w.into_inner();
    inner.flush()?;
    Ok(())
}

/// Write a self-closing tag.  `attrs` is a slice of `(key, value)` pairs;
/// values are escaped automatically.
pub fn empty<W: std::io::Write>(
    w: &mut Writer<W>,
    tag: &'static str,
    attrs: &[(&str, &str)],
) -> anyhow::Result<()> {
    let mut elem = BytesStart::new(tag);
    for (k, v) in attrs {
        elem.push_attribute((*k, *v));
    }
    w.write_event(Event::Empty(elem))?;
    Ok(())
}

/// Open a tag (no attributes).
pub fn open<W: std::io::Write>(w: &mut Writer<W>, tag: &'static str) -> anyhow::Result<()> {
    w.write_event(Event::Start(BytesStart::new(tag)))?;
    Ok(())
}

/// Open a tag with attributes.
pub fn open_attrs<W: std::io::Write>(
    w: &mut Writer<W>,
    tag: &'static str,
    attrs: &[(&str, &str)],
) -> anyhow::Result<()> {
    let mut elem = BytesStart::new(tag);
    for (k, v) in attrs {
        elem.push_attribute((*k, *v));
    }
    w.write_event(Event::Start(elem))?;
    Ok(())
}

/// Close a tag.
pub fn close<W: std::io::Write>(w: &mut Writer<W>, tag: &'static str) -> anyhow::Result<()> {
    w.write_event(Event::End(BytesEnd::new(tag)))?;
    Ok(())
}

/// Write a tag with text content (e.g. `<doc>text</doc>`).
pub fn text_tag<W: std::io::Write>(
    w: &mut Writer<W>,
    tag: &'static str,
    content: &str,
) -> anyhow::Result<()> {
    w.write_event(Event::Start(BytesStart::new(tag)))?;
    w.write_event(Event::Text(BytesText::new(content)))?;
    w.write_event(Event::End(BytesEnd::new(tag)))?;
    Ok(())
}
