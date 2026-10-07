//! MPL2 (.mpl) parser and writer.
//!
//! Simple line-based format with deciseconds (1/10 s) timing:
//!
//! ```text
//! [0][25]Hello|world
//! [30][60]/italic line
//! ```
//!
//! * `[start_ds][end_ds]text` per line.
//! * `|` is a hard line break.
//! * A leading `/` on any line (after a `|` split) means that line is
//!   italic.
//!
//! No other inline formatting; unknown content is preserved verbatim.

use std::collections::VecDeque;

use oxideav_core::{
    CodecId, CodecParameters, Error, Frame, MediaType, Packet, Result, Segment, SubtitleCue,
    TimeBase,
};
use oxideav_core::{Decoder, Encoder};

use crate::ir::SubtitleTrack;

pub const CODEC_ID: &str = "mpl2";

/// Parse an MPL2 payload.
pub fn parse(bytes: &[u8]) -> Result<SubtitleTrack> {
    let text = crate::encoding::decode_subtitle_text(bytes);
    let mut cues: Vec<SubtitleCue> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        let (start_ds, end_ds, rest) = match parse_header(line) {
            Some(v) => v,
            None => continue,
        };
        cues.push(SubtitleCue {
            start_us: ds_to_us(start_ds),
            end_us: if end_ds < 0 { -1 } else { ds_to_us(end_ds) },
            style_ref: None,
            positioning: None,
            segments: parse_inline(rest),
        });
    }
    cues.sort_by_key(|cue| cue.start_us);
    for index in 0..cues.len() {
        if cues[index].end_us < 0 {
            // An absent end lasts until the next cue. At EOF, FFmpeg's
            // negative duration becomes an unsigned millisecond duration.
            cues[index].end_us = cues.get(index + 1).map_or_else(
                || cues[index].start_us.saturating_add(i64::from(-100_i32 as u32) * 1000),
                |next| next.start_us,
            );
        }
    }
    Ok(SubtitleTrack {
        cues,
        ..SubtitleTrack::default()
    })
}

/// Re-emit a track as MPL2 bytes.
pub fn write(track: &SubtitleTrack) -> Result<Vec<u8>> {
    let mut out = String::new();
    for cue in &track.cues {
        let sd = us_to_ds(cue.start_us);
        let ed = us_to_ds(cue.end_us);
        out.push_str(&format!("[{}][{}]", sd, ed));
        out.push_str(&render_inline(&cue.segments));
        out.push('\n');
    }
    Ok(out.into_bytes())
}

/// Quick probe: look for the `[n][n]...` shape.
pub fn probe(buf: &[u8]) -> u8 {
    let text = crate::encoding::decode_subtitle_text(buf);
    let mut checked = 0;
    let mut hits = 0;
    for raw in text.split('\n') {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        checked += 1;
        if parse_header(line).is_some() {
            hits += 1;
        }
        if checked >= 5 {
            break;
        }
    }
    if checked == 0 {
        return 0;
    }
    if hits == checked {
        80
    } else if hits > 0 {
        40
    } else {
        0
    }
}

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    if params.codec_id.as_str() != CODEC_ID {
        return Err(Error::unsupported(format!(
            "not an mpl2 codec id: {}",
            params.codec_id.as_str()
        )));
    }
    Ok(Box::new(Mpl2Decoder {
        codec_id: params.codec_id.clone(),
        pending: VecDeque::new(),
        eof: false,
    }))
}

pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    if params.codec_id.as_str() != CODEC_ID {
        return Err(Error::unsupported(format!(
            "not an mpl2 codec id: {}",
            params.codec_id.as_str()
        )));
    }
    let mut p = params.clone();
    p.media_type = MediaType::Subtitle;
    Ok(Box::new(Mpl2Encoder {
        params: p,
        pending: VecDeque::new(),
    }))
}

// ---------------------------------------------------------------------------
// Packet shape

pub(crate) fn cue_to_bytes(cue: &SubtitleCue) -> Vec<u8> {
    let sd = us_to_ds(cue.start_us);
    let ed = us_to_ds(cue.end_us);
    let mut s = format!("[{}][{}]", sd, ed);
    s.push_str(&render_inline(&cue.segments));
    s.into_bytes()
}

pub(crate) fn bytes_to_cue(bytes: &[u8]) -> Result<SubtitleCue> {
    let text = crate::encoding::decode_subtitle_text(bytes);
    let line = text
        .lines()
        .map(|l| l.trim_end_matches('\r').trim())
        .find(|l| !l.is_empty())
        .ok_or_else(|| Error::invalid("mpl2: empty cue"))?;
    let (sd, ed, rest) =
        parse_header(line).ok_or_else(|| Error::invalid("mpl2: bad cue header"))?;
    Ok(SubtitleCue {
        start_us: ds_to_us(sd),
        end_us: if ed < 0 {
            ds_to_us(sd).saturating_add(i64::from(-100_i32 as u32) * 1000)
        } else {
            ds_to_us(ed)
        },
        style_ref: None,
        positioning: None,
        segments: parse_inline(rest),
    })
}

// ---------------------------------------------------------------------------

fn parse_header(line: &str) -> Option<(i64, i64, &str)> {
    let line = line.trim_start();
    let rest = line.strip_prefix('[')?;
    let e1 = rest.find(']')?;
    let ds1: i64 = rest[..e1].trim().parse().ok()?;
    let after1 = &rest[e1 + 1..];
    let after1 = after1.strip_prefix('[')?;
    let e2 = after1.find(']')?;
    let end = after1[..e2].trim();
    let ds2: i64 = if end.is_empty() { -1 } else { end.parse().ok()? };
    if ds1 < 0 || ds2 < -1 {
        return None;
    }
    ds1.checked_mul(100_000)?;
    if ds2 >= 0 {
        ds2.checked_mul(100_000)?;
    }
    Some((ds1, ds2, &after1[e2 + 1..]))
}

fn parse_inline(body: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    for (idx, piece) in body.split('|').enumerate() {
        if idx > 0 {
            out.push(Segment::LineBreak);
        }
        let mut text = piece.trim_start();
        let mut style = 0u8;
        while let Some(marker) = text.as_bytes().first() {
            style |= match marker {
                b'/' => 1,
                b'\\' => 2,
                b'_' => 4,
                _ => break,
            };
            text = &text[1..];
        }
        if !text.is_empty() {
            let mut segment = Segment::Text(text.to_string());
            if style & 1 != 0 { segment = Segment::Italic(vec![segment]); }
            if style & 2 != 0 { segment = Segment::Bold(vec![segment]); }
            if style & 4 != 0 { segment = Segment::Underline(vec![segment]); }
            out.push(segment);
        }
    }
    out
}

fn render_inline(segments: &[Segment]) -> String {
    // MPL2's style markers apply to an entire line.
    let mut lines: Vec<String> = vec![String::new()];
    let mut line_styles: Vec<u8> = vec![0];
    append_flat(segments, &mut lines, &mut line_styles, 0);
    let mut out = String::new();
    for (i, (line, style)) in lines.iter().zip(line_styles.iter()).enumerate() {
        if i > 0 {
            out.push('|');
        }
        if !line.is_empty() {
            if style & 1 != 0 { out.push('/'); }
            if style & 2 != 0 { out.push('\\'); }
            if style & 4 != 0 { out.push('_'); }
        }
        // `|` in text is reserved as a line break; sanitise.
        out.push_str(&line.replace('|', "/"));
    }
    out
}

fn append_flat(
    segments: &[Segment],
    lines: &mut Vec<String>,
    line_styles: &mut Vec<u8>,
    style: u8,
) {
    for seg in segments {
        match seg {
            Segment::Text(s) => {
                let idx = lines.len() - 1;
                lines[idx].push_str(s);
                if !s.is_empty() {
                    line_styles[idx] |= style;
                }
            }
            Segment::LineBreak => {
                lines.push(String::new());
                line_styles.push(0);
            }
            Segment::Italic(c) => append_flat(c, lines, line_styles, style | 1),
            Segment::Bold(c) => append_flat(c, lines, line_styles, style | 2),
            Segment::Underline(c) => append_flat(c, lines, line_styles, style | 4),
            Segment::Strike(c) => append_flat(c, lines, line_styles, style),
            Segment::Color { children, .. }
            | Segment::Font { children, .. }
            | Segment::Voice { children, .. }
            | Segment::Class { children, .. }
            | Segment::Karaoke { children, .. } => {
                append_flat(children, lines, line_styles, style)
            }
            Segment::Timestamp { .. } => {}
            Segment::Raw(_) => {}
        }
    }
}

// ---------------------------------------------------------------------------

fn ds_to_us(ds: i64) -> i64 {
    ds * 100_000
}

fn us_to_ds(us: i64) -> i64 {
    ((i128::from(us) + 50_000) / 100_000) as i64
}

// ---------------------------------------------------------------------------

struct Mpl2Decoder {
    codec_id: CodecId,
    pending: VecDeque<Frame>,
    eof: bool,
}

impl Decoder for Mpl2Decoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }
    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        let mut cue = bytes_to_cue(&packet.data)?;
        if let Some(pts) = packet.pts {
            let us = packet.time_base.rescale(pts, TimeBase::new(1, 1_000_000));
            let span = cue.end_us.saturating_sub(cue.start_us);
            cue.start_us = us;
            cue.end_us = us.saturating_add(span);
        }
        self.pending.push_back(Frame::Subtitle(cue));
        Ok(())
    }
    fn receive_frame(&mut self) -> Result<Frame> {
        if let Some(f) = self.pending.pop_front() {
            return Ok(f);
        }
        if self.eof {
            return Err(Error::Eof);
        }
        Err(Error::NeedMore)
    }
    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }
    fn reset(&mut self) -> Result<()> {
        self.pending.clear();
        self.eof = false;
        Ok(())
    }
}

struct Mpl2Encoder {
    params: CodecParameters,
    pending: VecDeque<Packet>,
}

impl Encoder for Mpl2Encoder {
    fn codec_id(&self) -> &CodecId {
        &self.params.codec_id
    }
    fn output_params(&self) -> &CodecParameters {
        &self.params
    }
    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        let cue = match frame {
            Frame::Subtitle(c) => c,
            _ => return Err(Error::invalid("mpl2 encoder: expected Frame::Subtitle")),
        };
        let tb = TimeBase::new(1, 1_000_000);
        let payload = cue_to_bytes(cue);
        let mut pkt = Packet::new(0, tb, payload);
        pkt.pts = Some(cue.start_us);
        pkt.dts = Some(cue.start_us);
        pkt.duration = Some((cue.end_us - cue.start_us).max(0));
        pkt.flags.keyframe = true;
        self.pending.push_back(pkt);
        Ok(())
    }
    fn receive_packet(&mut self) -> Result<Packet> {
        self.pending.pop_front().ok_or(Error::NeedMore)
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic() {
        let src = "[0][25]Hello world\n[30][60]Second line\n";
        let t = parse(src.as_bytes()).unwrap();
        assert_eq!(t.cues.len(), 2);
        assert_eq!(t.cues[0].start_us, 0);
        assert_eq!(t.cues[0].end_us, 2_500_000);
        assert_eq!(t.cues[1].start_us, 3_000_000);
    }

    #[test]
    fn parse_italic_prefix() {
        let src = "[0][10]/just italic\n";
        let t = parse(src.as_bytes()).unwrap();
        match &t.cues[0].segments[0] {
            Segment::Italic(_) => {}
            other => panic!("expected italic: {other:?}"),
        }
    }

    #[test]
    fn line_break() {
        let src = "[0][10]line1|line2\n";
        let t = parse(src.as_bytes()).unwrap();
        assert!(t.cues[0]
            .segments
            .iter()
            .any(|s| matches!(s, Segment::LineBreak)));
    }

    #[test]
    fn probe_positive() {
        assert!(probe(b"[0][10]hi\n") > 0);
    }

    #[test]
    fn probe_negative() {
        assert_eq!(probe(b"random\ntext\n"), 0);
    }
}
