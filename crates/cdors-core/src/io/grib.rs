//! GRIB2 messages decoded in cdors: the chunks of kerchunk references made by gribscan (codec
//! `gribscan.rawgrib`), where each chunk is one complete message holding one field.
//!
//! Supported: GRIB edition 2 with one field per message, data representation templates 5.0
//! (simple packing) and 5.42 (CCSDS, decoded by libaec, linked statically), with or without a bit-map
//! (section 6; points without a value become NaN). Values come out in the message's own order,
//! as ecCodes' `values` key gives them (the order of gribscan's coordinates), and are computed
//! as ecCodes computes them, `(X * 2^E + R) * 10^-D` with the scale factors built by repeated
//! multiplication, so that they equal what cdo and gribscan read bit for bit.

/// Decodes one GRIB message into its values.
pub fn decode(msg: &[u8]) -> Result<Vec<f64>, String> {
    if msg.len() < 16 || &msg[..4] != b"GRIB" {
        return Err("not a GRIB message".into());
    }
    match msg[7] {
        2 => decode2(msg),
        e => Err(format!("GRIB edition {e} is not supported")),
    }
}

fn be_u16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// GRIB's signed 16-bit integers: sign bit, then magnitude.
fn sm16(b: &[u8]) -> i32 {
    let v = i32::from(be_u16(b));
    if v & 0x8000 != 0 { -(v & 0x7fff) } else { v }
}

/// `n^s` as ecCodes' `codes_power` builds it (repeated multiplication or division).
fn power(s: i32, n: f64) -> f64 {
    let mut x = 1.0;
    for _ in 0..s.unsigned_abs() {
        if s < 0 {
            x /= n;
        } else {
            x *= n;
        }
    }
    x
}

/// The packed integers of simple packing, `n` values of `nbits` bits, most significant first,
/// handed to `f` one by one.
fn unpack_bits(data: &[u8], nbits: u32, n: usize, mut f: impl FnMut(u64)) -> Result<(), String> {
    if nbits > 64 {
        return Err(format!("{nbits} bits per value"));
    }
    let need = (n as u64 * u64::from(nbits)).div_ceil(8) as usize;
    if data.len() < need {
        return Err(format!(
            "{} data bytes for {n} values of {nbits} bits",
            data.len()
        ));
    }
    let mut bit = 0u64;
    for _ in 0..n {
        let mut v = 0u64;
        let mut left = nbits;
        while left > 0 {
            let byte = data[(bit / 8) as usize];
            let off = (bit % 8) as u32;
            let take = left.min(8 - off);
            let part = (byte >> (8 - off - take)) & ((1u16 << take) - 1) as u8;
            v = (v << take) | u64::from(part);
            left -= take;
            bit += u64::from(take);
        }
        f(v);
    }
    Ok(())
}

/// libaec's stream (`libaec.h`).
#[repr(C)]
struct AecStream {
    next_in: *const u8,
    avail_in: usize,
    total_in: usize,
    next_out: *mut u8,
    avail_out: usize,
    total_out: usize,
    bits_per_sample: std::ffi::c_uint,
    block_size: std::ffi::c_uint,
    rsi: std::ffi::c_uint,
    flags: std::ffi::c_uint,
    state: *mut std::ffi::c_void,
}

unsafe extern "C" {
    fn aec_buffer_decode(strm: *mut AecStream) -> std::ffi::c_int;
}

const AEC_DATA_3BYTE: u32 = 2;
const AEC_DATA_MSB: u32 = 4;

thread_local! {
    /// Decoded CCSDS samples, reused: a fresh multi-megabyte buffer per message costs page
    /// faults.
    static AEC_OUT: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The packed integers of CCSDS packing (template 5.42), handed to `f` one by one. The GRIB
/// flags describe the stream; the samples are asked for in 1, 2 or 4 little-endian bytes (as
/// ecCodes does), whatever the flags say about the layout the encoder was given.
fn unpack_ccsds(
    s5: &[u8],
    data: &[u8],
    nbits: u32,
    n: usize,
    mut f: impl FnMut(u64),
) -> Result<(), String> {
    if s5.len() < 25 || !(1..=32).contains(&nbits) {
        return Err(format!("template 5.42 with {nbits} bits per value"));
    }
    let size = match nbits {
        1..=8 => 1,
        9..=16 => 2,
        _ => 4,
    };
    AEC_OUT.with_borrow_mut(|buf| {
        let need = n * size;
        if buf.len() < need {
            buf.resize(need, 0);
        }
        let out = &mut buf[..need];
        let mut strm = AecStream {
            next_in: data.as_ptr(),
            avail_in: data.len(),
            total_in: 0,
            next_out: out.as_mut_ptr(),
            avail_out: need,
            total_out: 0,
            bits_per_sample: nbits,
            block_size: u32::from(s5[22]),
            rsi: u32::from(be_u16(&s5[23..25])),
            flags: u32::from(s5[21]) & !(AEC_DATA_3BYTE | AEC_DATA_MSB),
            state: std::ptr::null_mut(),
        };
        // SAFETY: the stream points at `data` and `out` with their true lengths, both outlive
        // the call, and `aec_buffer_decode` frees its own state before it returns.
        let rc = unsafe { aec_buffer_decode(&mut strm) };
        if rc != 0 || strm.total_out != need {
            return Err(format!(
                "CCSDS: libaec error {rc}, {} of {need} bytes decoded",
                strm.total_out
            ));
        }
        match size {
            1 => out.iter().for_each(|&b| f(u64::from(b))),
            2 => out
                .as_chunks::<2>()
                .0
                .iter()
                .for_each(|c| f(u64::from(u16::from_le_bytes(*c)))),
            _ => out
                .as_chunks::<4>()
                .0
                .iter()
                .for_each(|c| f(u64::from(u32::from_le_bytes(*c)))),
        }
        Ok(())
    })
}

fn decode2(msg: &[u8]) -> Result<Vec<f64>, String> {
    let total = u64::from_be_bytes(msg[8..16].try_into().expect("8 bytes")) as usize;
    if total > msg.len() {
        return Err(format!("message of {total} bytes cut to {}", msg.len()));
    }
    let (mut s3, mut s5, mut s6, mut s7) = (None, None, None, None);
    let mut pos = 16;
    while pos + 4 <= total && &msg[pos..pos + 4] != b"7777" {
        if pos + 5 > total {
            return Err("truncated section".into());
        }
        let len = be_u32(&msg[pos..]) as usize;
        let num = msg[pos + 4];
        if len < 5 || pos + len > total {
            return Err(format!("section {num} of {len} bytes overruns the message"));
        }
        let sec = &msg[pos..pos + len];
        match num {
            3 => s3 = Some(sec),
            5 => s5 = Some(sec),
            6 => s6 = Some(sec),
            7 if s7.is_some() => return Err("more than one field in the message".into()),
            7 => s7 = Some(sec),
            _ => {}
        }
        pos += len;
    }
    let (Some(s3), Some(s5), Some(s6), Some(s7)) = (s3, s5, s6, s7) else {
        return Err("sections 3, 5, 6 or 7 missing".into());
    };
    if s3.len() < 10 || s5.len() < 21 || s6.len() < 6 {
        return Err("section too short".into());
    }
    let npoints = be_u32(&s3[6..]) as usize;
    let nvalues = be_u32(&s5[5..]) as usize;
    let template = be_u16(&s5[9..]);
    let reference = f64::from(f32::from_bits(be_u32(&s5[11..])));
    let bscale = power(sm16(&s5[15..]), 2.0);
    let dscale = power(-sm16(&s5[17..]), 10.0);
    let nbits = u32::from(s5[19]);
    let data = &s7[5..];
    let value = |x: u64| (x as f64 * bscale + reference) * dscale;
    let mut vals = Vec::with_capacity(nvalues);
    match (template, nbits) {
        // a constant field: no data
        (0 | 42, 0) => vals.resize(nvalues, value(0)),
        (0, _) => unpack_bits(data, nbits, nvalues, |x| vals.push(value(x)))?,
        (42, _) => unpack_ccsds(s5, data, nbits, nvalues, |x| vals.push(value(x)))?,
        (t, _) => {
            return Err(format!(
                "data representation template 5.{t} is not supported"
            ));
        }
    }
    match s6[5] {
        255 if nvalues == npoints => Ok(vals),
        255 => Err(format!(
            "{nvalues} values for {npoints} points without a bit-map"
        )),
        0 => {
            let bits = &s6[6..];
            if bits.len() * 8 < npoints {
                return Err("bit-map shorter than the grid".into());
            }
            let mut it = vals.into_iter();
            let mut out = Vec::with_capacity(npoints);
            for i in 0..npoints {
                out.push(if bits[i / 8] & (0x80 >> (i % 8)) != 0 {
                    it.next()
                        .ok_or("bit-map marks more points than there are values")?
                } else {
                    f64::NAN
                });
            }
            Ok(out)
        }
        b => Err(format!("bit-map indicator {b} is not supported")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_packing_with_a_bitmap() {
        // 4 points, 3 values of 4 bits (1, 2, 15), point 2 missing; R = 10, E = 1, D = 1
        let mut m = b"GRIB\0\0\0\x02".to_vec();
        let s3 = [0u8, 0, 0, 14, 3, 0, 0, 0, 0, 4, 0, 0, 0, 0];
        let mut s5 = vec![0u8, 0, 0, 21, 5, 0, 0, 0, 3, 0, 0];
        s5.extend_from_slice(&10f32.to_bits().to_be_bytes());
        s5.extend_from_slice(&[0, 1, 0, 1, 4, 0]);
        let s6 = [0u8, 0, 0, 7, 6, 0, 0b1011_0000];
        let s7 = [0u8, 0, 0, 7, 7, 0x12, 0xf0];
        let total = 16 + s3.len() + s5.len() + s6.len() + s7.len() + 4;
        m.extend_from_slice(&(total as u64).to_be_bytes());
        for s in [&s3[..], &s5, &s6, &s7, b"7777"] {
            m.extend_from_slice(s);
        }
        let v = decode(&m).unwrap();
        let want = [
            (1.0 * 2.0 + 10.0) * 0.1,
            f64::NAN,
            (2.0 * 2.0 + 10.0) * 0.1,
            (15.0 * 2.0 + 10.0) * 0.1,
        ];
        assert_eq!(v.len(), 4);
        for (a, b) in v.iter().zip(want) {
            assert!(a == &b || (a.is_nan() && b.is_nan()), "{a} vs {b}");
        }
    }
}
