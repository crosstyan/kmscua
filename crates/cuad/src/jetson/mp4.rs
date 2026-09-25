//! Minimal MP4 writer for one constant-rate H.264 track.
//!
//! Enough for what the encoder emits (Annex-B, no B-frames, SPS/PPS before
//! each IDR): `ftyp`, one `mdat` streamed to disk, and `moov` written on
//! finish with one sample per chunk and 64-bit chunk offsets. Like `mp4mux`,
//! a file that never reaches `finish` has no index.

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::{bail, Context, Result};

pub struct Mp4Writer {
    out: BufWriter<File>,
    width: u32,
    height: u32,
    fps: u32,
    mdat_start: u64,
    pos: u64,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    sizes: Vec<u32>,
    offsets: Vec<u64>,
    keys: Vec<u32>,
}

impl Mp4Writer {
    pub fn create(path: &Path, width: u32, height: u32, fps: u32) -> Result<Self> {
        let file = File::create(path).with_context(|| format!("create {}", path.display()))?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let mut ftyp = Vec::new();
        ftyp.extend_from_slice(b"isom");
        ftyp.extend_from_slice(&512u32.to_be_bytes());
        for b in [b"isom", b"iso2", b"avc1", b"mp41"] {
            ftyp.extend_from_slice(b);
        }
        let ftyp = boxed(b"ftyp", &ftyp);
        out.write_all(&ftyp)?;
        // mdat with a 64-bit size, patched in finish().
        let mdat_start = ftyp.len() as u64;
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        out.write_all(&0u64.to_be_bytes())?;
        Ok(Self {
            out,
            width,
            height,
            fps: fps.max(1),
            mdat_start,
            pos: mdat_start + 16,
            sps: None,
            pps: None,
            sizes: Vec::new(),
            offsets: Vec::new(),
            keys: Vec::new(),
        })
    }

    pub fn frames(&self) -> usize {
        self.sizes.len()
    }

    /// Append one access unit in Annex-B form as the next sample.
    pub fn write_annexb(&mut self, au: &[u8]) -> Result<()> {
        let mut size = 0u32;
        let mut key = false;
        let offset = self.pos;
        for nal in nal_units(au) {
            match nal[0] & 0x1f {
                7 => {
                    self.sps.get_or_insert_with(|| nal.to_vec());
                    continue;
                }
                8 => {
                    self.pps.get_or_insert_with(|| nal.to_vec());
                    continue;
                }
                9 => continue, // access unit delimiter
                5 => key = true,
                _ => {}
            }
            self.out.write_all(&(nal.len() as u32).to_be_bytes())?;
            self.out.write_all(nal)?;
            size += 4 + nal.len() as u32;
        }
        if size == 0 {
            return Ok(());
        }
        if self.sps.is_none() && self.sizes.is_empty() {
            bail!("stream does not start with SPS/PPS");
        }
        self.pos += size as u64;
        self.sizes.push(size);
        self.offsets.push(offset);
        if key {
            self.keys.push(self.sizes.len() as u32);
        }
        Ok(())
    }

    /// Patch the mdat size and append the index.
    pub fn finish(mut self) -> Result<()> {
        let (Some(sps), Some(pps)) = (self.sps.take(), self.pps.take()) else {
            bail!("no frames encoded");
        };
        if sps.len() < 4 {
            bail!("short SPS");
        }
        let moov = self.moov(&sps, &pps);
        self.out.write_all(&moov)?;
        self.out.seek(SeekFrom::Start(self.mdat_start + 8))?;
        self.out.write_all(&(self.pos - self.mdat_start).to_be_bytes())?;
        let file = self.out.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        Ok(())
    }

    fn moov(&self, sps: &[u8], pps: &[u8]) -> Vec<u8> {
        let n = self.sizes.len() as u32;
        // Track timescale fps*1000 with a sample delta of 1000 keeps every rate exact.
        let timescale = self.fps * 1000;
        let media_dur = n * 1000;
        let movie_dur = (n as u64 * 1000 / self.fps as u64) as u32; // ms

        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&[0; 8]); // creation, modification
        mvhd.extend_from_slice(&1000u32.to_be_bytes());
        mvhd.extend_from_slice(&movie_dur.to_be_bytes());
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
        mvhd.extend_from_slice(&[0; 10]);
        mvhd.extend_from_slice(&MATRIX);
        mvhd.extend_from_slice(&[0; 24]);
        mvhd.extend_from_slice(&2u32.to_be_bytes()); // next track id

        let mut tkhd = Vec::new();
        tkhd.extend_from_slice(&[0; 8]);
        tkhd.extend_from_slice(&1u32.to_be_bytes()); // track id
        tkhd.extend_from_slice(&[0; 4]);
        tkhd.extend_from_slice(&movie_dur.to_be_bytes());
        tkhd.extend_from_slice(&[0; 8]);
        tkhd.extend_from_slice(&[0; 8]); // layer, alternate group, volume, reserved
        tkhd.extend_from_slice(&MATRIX);
        tkhd.extend_from_slice(&(self.width << 16).to_be_bytes());
        tkhd.extend_from_slice(&(self.height << 16).to_be_bytes());

        let mut mdhd = Vec::new();
        mdhd.extend_from_slice(&[0; 8]);
        mdhd.extend_from_slice(&timescale.to_be_bytes());
        mdhd.extend_from_slice(&media_dur.to_be_bytes());
        mdhd.extend_from_slice(&0x55c4u16.to_be_bytes()); // "und"
        mdhd.extend_from_slice(&[0; 2]);

        let mut hdlr = Vec::new();
        hdlr.extend_from_slice(&[0; 4]);
        hdlr.extend_from_slice(b"vide");
        hdlr.extend_from_slice(&[0; 12]);
        hdlr.extend_from_slice(b"VideoHandler\0");

        let mut avcc = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        avcc.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(sps);
        avcc.push(1);
        avcc.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        avcc.extend_from_slice(pps);

        let mut avc1 = vec![0; 6];
        avc1.extend_from_slice(&1u16.to_be_bytes()); // data reference index
        avc1.extend_from_slice(&[0; 16]);
        avc1.extend_from_slice(&(self.width as u16).to_be_bytes());
        avc1.extend_from_slice(&(self.height as u16).to_be_bytes());
        avc1.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi
        avc1.extend_from_slice(&0x0048_0000u32.to_be_bytes());
        avc1.extend_from_slice(&[0; 4]);
        avc1.extend_from_slice(&1u16.to_be_bytes()); // frame count
        avc1.extend_from_slice(&[0; 32]); // compressor name
        avc1.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
        avc1.extend_from_slice(&(-1i16).to_be_bytes());
        avc1.extend_from_slice(&boxed(b"avcC", &avcc));

        let mut stsd = 1u32.to_be_bytes().to_vec();
        stsd.extend_from_slice(&boxed(b"avc1", &avc1));

        let mut stts = 1u32.to_be_bytes().to_vec();
        stts.extend_from_slice(&n.to_be_bytes());
        stts.extend_from_slice(&1000u32.to_be_bytes());

        let mut stss = (self.keys.len() as u32).to_be_bytes().to_vec();
        self.keys.iter().for_each(|k| stss.extend_from_slice(&k.to_be_bytes()));

        let mut stsc = 1u32.to_be_bytes().to_vec();
        for v in [1u32, 1, 1] {
            stsc.extend_from_slice(&v.to_be_bytes());
        }

        let mut stsz = 0u32.to_be_bytes().to_vec();
        stsz.extend_from_slice(&n.to_be_bytes());
        self.sizes.iter().for_each(|s| stsz.extend_from_slice(&s.to_be_bytes()));

        let mut co64 = n.to_be_bytes().to_vec();
        self.offsets.iter().for_each(|o| co64.extend_from_slice(&o.to_be_bytes()));

        let stbl = [
            full(b"stsd", 0, &stsd),
            full(b"stts", 0, &stts),
            full(b"stss", 0, &stss),
            full(b"stsc", 0, &stsc),
            full(b"stsz", 0, &stsz),
            full(b"co64", 0, &co64),
        ]
        .concat();
        let dref = full(b"dref", 0, &[1u32.to_be_bytes().to_vec(), full(b"url ", 1, &[])].concat());
        let minf = [
            full(b"vmhd", 1, &[0; 8]),
            boxed(b"dinf", &dref),
            boxed(b"stbl", &stbl),
        ]
        .concat();
        let mdia = [full(b"mdhd", 0, &mdhd), full(b"hdlr", 0, &hdlr), boxed(b"minf", &minf)].concat();
        let trak = [full(b"tkhd", 3, &tkhd), boxed(b"mdia", &mdia)].concat();
        boxed(b"moov", &[full(b"mvhd", 0, &mvhd), boxed(b"trak", &trak)].concat())
    }
}

const MATRIX: [u8; 36] = {
    let mut m = [0u8; 36];
    m[1] = 1; // a = 0x00010000
    m[17] = 1; // d = 0x00010000
    m[32] = 0x40; // w = 0x40000000
    m
};

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + body.len());
    v.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
    v.extend_from_slice(kind);
    v.extend_from_slice(body);
    v
}

fn full(kind: &[u8; 4], flags: u32, body: &[u8]) -> Vec<u8> {
    let mut v = (flags & 0x00ff_ffff).to_be_bytes().to_vec(); // version 0
    v.extend_from_slice(body);
    boxed(kind, &v)
}

/// Split an Annex-B buffer into NAL units (start codes removed).
fn nal_units(buf: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 && buf[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map(|&n| n - 3).unwrap_or(buf.len());
        // A 4-byte start code leaves its leading zero on the previous NAL.
        while e > s && buf[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&buf[s..e]);
        }
    }
    out.into_iter()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annexb() {
        let buf = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5];
        let nals: Vec<_> = nal_units(&buf).collect();
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
    }
}
